//! [`Supervisor`]: one `handle(Input) -> &[Effect]`, and the rules it encodes.
//!
//! Two shapes carry the whole crate.
//!
//! **The ledger.** Every record the machine wants written goes out as `Persist{token, entry}`
//! with the *action that record authorises* parked under that token in [`Ledger`]. Nothing is
//! folded and nothing is acted on until [`Input::Persisted`] brings the token back. That is
//! invariant 2 for turns — the parked action for a `TurnIssued` is the `Spawn` — and it is also
//! why a crash between emit and fsync cannot leave the in-memory world ahead of the journal.
//!
//! **The fold.** [`World::apply`] runs on the acknowledged entry, and `Input::Replayed` calls the
//! same function. Replay is not a second code path, so a decision taken after a crash is the
//! decision that would have been taken before it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use dudes_policy::{
    Admission, BreakerConfig, COMPACTION_EXEMPT, COMPACTION_GRACE_MS,
    COMPACTION_MAX_SUPPRESSED_BEATS, COMPACTION_TRAILING_GRACE_MS, Decided, LoopInput, WorkInput,
    admit,
};
use dudes_proto::goal::{Evidence, Predicate};
use dudes_proto::record::{BrowserOnboardingGoal, BrowserOnboardingHire};
use dudes_proto::reply::{HookDecision, HookReply};
use dudes_proto::rpc::{JSONRPC, Notification};
use dudes_proto::{
    Actor, Entry, Event, EventKind, ExitStatus, Gate, GateStep, Goal, KillDeferredReason,
    KillReason, LiveActivityKind, LoopDecision, Record, Spend, TurnOutcome,
};
use dudes_providers::adapter::{
    Adapter, AgentCtx, HookInstall, Isolation, ManagedInstall, TurnSession,
};
use dudes_providers::launch::{EnvVar, LaunchError, LaunchSpec};
use dudes_providers::mcp::{LoadedServers, Rules};
use dudes_providers::registry::Adapters;
use dudes_providers::stream::{Credential, ScopedUsage, SessionObservation};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, BreakerSignal, CardId, GoalId, Lsn, Mode, ModelSelection,
    Provenance, Provider, SessionId, ThreadId, Timestamp, TurnId, Usage, Usd, WorktreeId,
};

use crate::agent_fsm::{self, FsmEvent};
use crate::effect::Effect;
use crate::host::{AdoptionKey, Clock, Liveness, Rng, input_digest, session_id_from_ulid};
use crate::input::{
    BrowserAnswerCommand, ClientId, Command, HookId, Input, PersistToken, RuntimeSignal, SpendView,
};
use crate::token::Mint;
use crate::world::{AgentRow, MAX_PENDING_STEERS, World};

mod browser_notes;
mod conversations;
mod mail_wakes;
#[cfg(test)]
mod ticket_start;
use browser_notes::BrowserLaunchPhase;
use mail_wakes::{MailRefresh, MailRefreshPhase};

enum ConsumedNote {
    Local(Lsn),
    Browser(Lsn),
}

/// The current writer's cancellation result for one browser note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrowserNoteCancelDisposition {
    /// No issue owns the note.
    Queued,
    /// Every attempt is proven not dispatched.
    CleanIssue {
        /// The immutable issue.
        turn: TurnId,
    },
    /// A previous dispatch may have delivered text or consumed usage.
    AmbiguousReissue {
        /// The unresolved issue.
        turn: TurnId,
        /// The latest historical intent.
        dispatch_intent_lsn: Lsn,
    },
    /// The request can have no cancellation effect.
    Refused {
        /// Fixed refusal data only.
        reason: dudes_proto::ChatReason,
    },
}

/// What the daemon knows that is not in the journal: where its data lives, what environment a
/// child inherits, and the floor-wide ceiling from configuration.
///
/// Held for the whole daemon run, and `daemon_env` is the environment before any scrub, so an
/// operator's `ANTHROPIC_API_KEY` sits in it from boot. `daemon_env`'s values are
/// [`EnvVar`](dudes_providers::launch::EnvVar)s, which is what makes the `Debug` derive safe
/// over that field; no other field here carries a secret.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The daemon's data directory; an agent's own directory is `<data_dir>/agents/<AgentId>`.
    pub data_dir: PathBuf,
    /// The RPC socket path, handed to a child as `DUDES_SOCK`.
    pub socket: PathBuf,
    /// The environment a managed child starts from, before the adapter scrubs it.
    pub daemon_env: Vec<EnvVar>,
    /// `[budget] floor_usd_per_day` (gate 5), absent when the operator configured no floor
    /// ceiling.
    pub floor_usd_per_day: Option<Usd>,
    /// Configured price of each journaled unobserved end.
    pub per_turn_floor: Spend,
    /// `[breaker]`: whether the ladder may reach `Stop`. Defaults to `hard_stop = true`.
    pub breaker: BreakerConfig,
    /// The absolute UTF-8 path of `dudes-hook`, which an attached CLI's hook settings run. `None`,
    /// or a path the settings file cannot carry, refuses the attach: a CLI without its hooks would
    /// run ungoverned.
    pub hook_command: Option<PathBuf>,
    /// The daemon's one-entry directory holding only a `dudes` link to its sibling CLI. When set,
    /// a managed launch whose environment carries a `PATH` gets this directory in front of it.
    /// `None` leaves every `PATH` as it was; it is what a missing sibling, a failed link and
    /// Windows all look like.
    pub agent_bin: Option<PathBuf>,
    /// The operator's declared MCP servers as this run can launch them, built once at start. Its
    /// secret values are [`EnvVar`]s, so the `Debug` derive stays safe.
    pub mcp: LoadedServers,
}

/// Put `dir` in front of the `PATH` the child will receive and change nothing else: no variable is
/// added, none moves. A child without a `PATH`, or a directory `join_paths` refuses, is left as it
/// was. The value is built from typed parts, never from a shell string.
fn prepend_agent_bin(env: &mut [EnvVar], dir: &Path) {
    let Some(path) = env.iter_mut().find(|var| {
        if cfg!(windows) {
            var.name().as_encoded_bytes().eq_ignore_ascii_case(b"PATH")
        } else {
            var.name() == "PATH"
        }
    }) else {
        return;
    };
    let rest = std::env::split_paths(path.expose_value());
    let Ok(joined) = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(rest)) else {
        return;
    };
    *path = EnvVar::new(path.name().to_owned(), joined);
}

/// What an acknowledged `Persist` authorises. Parked under the persist token; run once, when the
/// entry is on the disk.
#[derive(Debug)]
enum Then {
    ConversationStartupReserved,
    ConversationRetryReserved {
        turn: TurnId,
        generation: u64,
    },
    ConversationIssue {
        agent: AgentId,
        note_lsn: Lsn,
        context_lsn: Lsn,
        task: String,
    },
    MailWakeContinue {
        agent: AgentId,
    },
    MailWakeCheckIssue {
        agent: AgentId,
        root_lsn: Lsn,
        at: Timestamp,
        input_digest: dudes_proto::MailWakeDigest,
    },
    TaughtBudgetRefused {
        audit: Box<Record>,
    },
    BrowserIssue {
        agent: AgentId,
        task: String,
        answers: Vec<Lsn>,
        note_lsn: Lsn,
        completion_lsn: Option<Lsn>,
    },
    BrowserDispatch {
        turn: TurnId,
        generation: u64,
    },
    CompletedCommandEnded {
        agent: AgentId,
    },
    NoteEnded {
        agent: AgentId,
    },
    AnswerSaved {
        agent: AgentId,
    },
    ClosingAnnounced,
    ClosingIssue {
        agent: AgentId,
        task: String,
        answers: Vec<Lsn>,
    },
    /// Nothing beyond the fold.
    Fold,
    /// The `LoopDecided{Continue}` landed: issue the turn it decided on, naming its `lsn` and
    /// the answers its task carries.
    Issue {
        agent: AgentId,
        task: String,
        answers: Vec<Lsn>,
    },
    /// The `TurnIssued` landed: decide the launch. **The only live-path entry to [`Supervisor::spawn`].**
    Spawn {
        agent: AgentId,
        turn: TurnId,
        launch: Box<PreparedLaunch>,
    },
    McpGranted {
        agent: AgentId,
        turn: TurnId,
        launch: Box<PreparedLaunch>,
    },
    /// The `HookTokenMinted` landed: push the `Spawn` whose claim on
    /// [`Supervisor::spawning`] is already made.
    ///
    /// **Not folded into [`Then::Spawn`], and the reason is that variant's own guard.** Its arm
    /// runs [`Supervisor::spawn`], which claims the turn in `spawning` and answers `false` for a
    /// turn already claimed — so re-entering it after the mint would refuse the very launch the
    /// mint exists to arm. The claim and the push are one decision taken at one moment
    /// ([`Supervisor::spawn`] still owns both); only the push is deferred, and this is what
    /// defers it. `Then::OpenPty` needs no twin because the attached path has no such guard: its
    /// arm is re-entered as itself.
    SpawnArmed {
        agent: AgentId,
        turn: TurnId,
        launch: Box<PreparedLaunch>,
        mcp_grant: Option<Lsn>,
    },
    /// A re-issued `TurnSpawned` landed: persist the current attempt's init that arrived before
    /// the spawn acknowledgement, then enforce any standing cause against the child.
    ReissueSpawned {
        agent: AgentId,
        turn: TurnId,
    },
    /// Re-decide for this agent.
    Decide {
        agent: AgentId,
    },
    /// A standing cause was lifted: re-run crash rule (2) for this agent's turns that are still
    /// waiting for a process, then decide. `Resume` lifts a halt, a pause or an automatic hold, and
    /// `GoalSet` lifts an automatic hold.
    ///
    /// **Not folded into [`Then::Decide`].** That variant also fires for `HumanAnswer`,
    /// `PredicateDone` and `PtyExited`, none of which lifts a cause, and re-issuing on all of
    /// them would launch from moments the rule was never written for.
    ClearedCause {
        agent: AgentId,
    },
    /// The `HumanQuestion` landed: tell the subscribers, then re-decide (gate 3 answers `Wait`).
    Question {
        agent: AgentId,
        text: String,
    },
    /// The `TurnEnded` landed: judge the goal, or re-decide when it has nothing to judge.
    Judge {
        agent: AgentId,
        goal: GoalId,
    },
    HoldAfterRapidFailureBeat {
        agent: AgentId,
        first: TurnId,
        latest: TurnId,
    },
    /// The `Attached` landed: open the PTY. Re-entered as itself when the agent's token had to be
    /// minted first, at which point the mint is what this is parked under.
    OpenPty {
        agent: AgentId,
        spec: Box<LaunchSpec>,
        install: HookInstall,
        cols: u16,
        rows: u16,
    },
    /// An isolation leak beat landed: refresh the floor, then enforce a terminal rung if one was
    /// reached for reasons already standing on the ladder.
    IsolationLeak {
        agent: AgentId,
        stop: bool,
    },
    /// A standing cause was journaled, or a child appeared under one: kill it if there is one.
    KillStandingCause {
        agent: AgentId,
    },
    Retired {
        agent: AgentId,
    },
    /// The `SteerConsumed` landed: answer the hook it was consumed for.
    ///
    /// **The reply is built before the persist and carried here unchanged.** It is not rebuilt on
    /// acknowledgement, because by then the fold has removed the note and a rebuilt reply would
    /// carry nothing — the consumption would be journaled and the correction never sent.
    Reply {
        hook: HookId,
        reply: HookReply,
    },
}

#[derive(Debug)]
struct ManagedEnd {
    elapsed_ms: Option<u64>,
    exit: ExitStatus,
    usage: Option<ScopedUsage>,
    model_usage: BTreeMap<String, Usage>,
    outcome: TurnOutcome,
    last_message: Option<String>,
    final_reply: Option<dudes_proto::ChatReply>,
}

impl ManagedEnd {
    fn absent(outcome: TurnOutcome) -> Self {
        Self {
            elapsed_ms: None,
            exit: ExitStatus::UNKNOWN,
            usage: None,
            model_usage: BTreeMap::new(),
            outcome,
            last_message: None,
            final_reply: None,
        }
    }
}

/// One `Persist` in flight.
#[derive(Debug)]
struct Parked {
    entry: Entry,
    then: Then,
}

/// The persist-before-spawn ledger.
type Ledger = BTreeMap<PersistToken, Parked>;

/// The private result of composing one managed launch.
///
/// Absence and refusal have different callers: live issue preserves both as a pre-`TurnIssued`
/// stop, while replay turns only a typed adapter refusal into a durable no-child end.
#[derive(Debug)]
enum ManagedLaunch {
    Ready(Box<PreparedLaunch>),
    Unavailable,
    Refused(LaunchError),
}

#[derive(Debug)]
struct PreparedLaunch {
    spec: LaunchSpec,
    install: ManagedInstall,
    policy_lsn: Lsn,
    task: String,
    session: TurnSession,
    selection: ModelSelection,
}

#[derive(Debug)]
struct McpAttempt {
    grant: Option<Lsn>,
    key: Option<AdoptionKey>,
    observations: Vec<(AdoptionKey, SessionObservation)>,
}

fn same_child_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
}

/// What the machine remembers about a child it can still reach: the key a `Kill` or a `Probe`
/// needs. The agent and the goal are on the turn row; nothing is duplicated here.
#[derive(Debug)]
struct Child {
    key: AdoptionKey,
}

/// What the reaper last answered about one open child, with the start of an unresolved run.
///
/// `Gone` is deliberately absent: that answer ends the turn instead of being retained. Encoding
/// the other two answers as variants keeps an `Alive` answer from carrying an `Unknown` stamp and
/// makes clearing the stamp the state transition itself rather than a second map mutation.
#[derive(Clone, Copy, Debug)]
enum LivenessState {
    Alive,
    Unknown { first_at: Timestamp },
}

impl LivenessState {
    const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown { .. })
    }
}

/// The shortest gap between two `LiveActivityObserved` records for one agent. It bounds the
/// journal when an agent's newest kind alternates at every one-second flush.
const ACTIVITY_SPACING_MS: i64 = 5_000;

/// How long an unchanged kind waits before a fresh sample of it is journaled again. Below the
/// Floor's 60-second display lifetime by more than its five-second browser allowance and the
/// one-second flush, so a kind that keeps being observed never lapses on screen.
const ACTIVITY_REFRESH_MS: i64 = 30_000;

/// The last `LiveActivityObserved` folded for one agent.
#[derive(Clone, Copy, Debug)]
struct JournaledActivity {
    /// `None` once a lifecycle record has replaced the projected kind, so the next sample is a
    /// change even when it repeats this one.
    kind: Option<LiveActivityKind>,
    at: Timestamp,
}

/// One agent's run-local compaction exemption.
///
/// Neither field is journalled. A restart forgets both and therefore fails toward counting the
/// next signal. The deadline bounds a lost end edge; the suppressed-beat count bounds repeated
/// start edges from an authorised caller.
#[derive(Debug, Default)]
struct Compaction {
    until: Option<Timestamp>,
    suppressed_beats: u8,
}

impl Compaction {
    fn is_live(&self, now: Timestamp) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    fn start(&mut self, now: Timestamp) {
        if !self.is_live(now) {
            self.until = Some(now.saturating_add_millis(COMPACTION_GRACE_MS));
        }
    }

    fn end(&mut self, now: Timestamp) {
        if self.is_live(now) {
            let trailing = now.saturating_add_millis(COMPACTION_TRAILING_GRACE_MS);
            self.until = self.until.map(|until| until.min(trailing));
        }
    }

    /// Resolve the pure policy's boolean and account for what this beat actually suppressed.
    fn resolve(
        &mut self,
        now: Timestamp,
        signals: &[BreakerSignal],
        max_suppressed_beats: u8,
    ) -> bool {
        let exempt_seen = signals
            .iter()
            .any(|signal| COMPACTION_EXEMPT.contains(signal));
        let compacting = self.is_live(now) && self.suppressed_beats < max_suppressed_beats;
        if compacting && exempt_seen {
            self.suppressed_beats = self.suppressed_beats.saturating_add(1);
        } else if !exempt_seen {
            // The first beat with no exempt signal is the agent's evidence of life. Expiry, a
            // new start and a spent ceiling deliberately do not reach this arm.
            self.suppressed_beats = 0;
        }
        compacting
    }
}

/// The machine.
///
/// Generic over the clock and the randomness so the tests hold both still; the process host is
/// *not* a parameter, because `handle()` never calls it — the runtime does, against the effects
/// this returns.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the browser-delivery latch is distinct from closing recovery and activity sampling"
)]
pub struct Supervisor<C: Clock, R: Rng> {
    config: Config,
    /// This run's adapters, built once from the operator's `[providers]` table.
    ///
    /// The **same** value the daemon's `agent.hire` reads before it admits a hire. A hire on a
    /// provider absent here is refused at the door rather than journaled, because
    /// [`Self::launch`] would find nothing and the agent could never take a turn — so the two
    /// readers ask one registry instead of two maps that can drift.
    adapters: Adapters,
    clock: C,
    rng: R,
    world: World,
    closing_active: bool,
    closing_recovery: bool,
    /// Agents whose recorded worktree is missing, as [`Input::MissingWorktrees`] set them.
    missing_worktrees: BTreeSet<AgentId>,
    status: BTreeMap<AgentId, AgentStatus>,
    ledger: Ledger,
    spend: BTreeMap<AgentId, SpendView>,
    mail_refreshes: BTreeMap<AgentId, MailRefresh>,
    next_mail_request: u64,
    /// Gate 7's card, mail count and memory digest per agent, as the caller last pushed them on
    /// [`Input::Work`]. Run-local like [`Self::spend`] and journaled nowhere: an agent with no
    /// entry gets `WorkInput::default()`, which is the goal text alone.
    work: BTreeMap<AgentId, WorkInput>,
    goals_awaiting_board_result: BTreeSet<GoalId>,
    children: BTreeMap<TurnId, Child>,
    mcp_attempts: BTreeMap<TurnId, McpAttempt>,
    /// Turns this machine emitted a `Kill` for, so their end is `Killed{by}` and not whatever the
    /// adapter read off a signalled process.
    kills: BTreeMap<TurnId, KillReason>,
    /// Turns a re-issue has parked a `Spawn` for, against the session that re-issue **announced**.
    ///
    /// A map and not a set, because `TurnSpawned.session` must be the id the turn was *launched*
    /// on rather than the one the projection happened to hold, and `TurnSpawned.reissued` is
    /// derived from whether the lookup found the **key** — so a re-issue with no entry would be
    /// journaled `reissued: false`. Run-local, not persistent state: after a crash the turn is
    /// `Issued` again and announces again.
    reissued: BTreeMap<TurnId, Option<SessionId>>,
    /// The first accepted session report for a current re-issue that arrived before its
    /// `TurnSpawned` was durable. It is persisted only after that record clears the prior
    /// attempt's session and confirmation flag.
    reissue_confirmations: BTreeMap<TurnId, SessionId>,
    /// Turns this run emitted an `Effect::Spawn` for whose `TurnSpawned` is not yet acknowledged.
    ///
    /// Invariant 2's run-local half, and the guard inside [`Self::spawn`]. It cannot live on
    /// [`World`]: the world is the fold of *fsynced* records, so it calls a turn `Issued` whether
    /// or not a launch for it is already outstanding — `TurnSpawned` is not written until the host
    /// answers. Empty after a restart is the correct value, because the journal carries the
    /// persistent half: a folded `TurnSpawned` takes the turn out of `issued_not_spawned()`.
    ///
    /// **The consequence, stated rather than discovered:** a launch the host never answers leaves
    /// its turn here for the rest of the run, and no cleared cause re-issues it. The next start
    /// clears the set and the second crash rule issues it once. Stuck-but-visible, which is the shape
    /// chosen over a second process.
    spawning: BTreeSet<TurnId>,
    browser_delivery_open: bool,
    browser_launches: BTreeMap<TurnId, BrowserLaunchPhase>,
    browser_generation: u64,
    browser_context: BTreeMap<AgentId, Lsn>,
    /// The note each agent has a `SteerConsumed` in flight for, by the position it names.
    /// At most one per agent, because `pending_steer` yields nothing while an entry
    /// is here.
    ///
    /// `gate_reply` yields no note for an agent in here, so a second admitted event arriving
    /// inside the fsync window cannot read a queue the journal has already moved past and deliver
    /// the same note twice. Same reason as [`Supervisor::spawning`] above, and not persistent
    /// state: it dies with the process, and replay rebuilds the truth from the records.
    ///
    /// **It carries the position, not just the agent, because the cap has to know which note is
    /// leaving.** A set would say "this agent is mid-consume" and not "note *n* is spent", and
    /// [`Supervisor::steer_evictions`] needs the second to avoid naming a note the journal has
    /// already spent — or, having excluded it, to avoid evicting a live one in its place.
    steering: BTreeMap<AgentId, Lsn>,
    /// Positions this run has named in a `SteerCleared` that is persisted but not yet folded.
    ///
    /// The eviction the cap requires is chosen from the **acknowledged** queue, so without this a
    /// second `agent.steer` arriving inside the first one's fsync window would name the same
    /// front note again — and the second clear, being idempotent, would fold to nothing and leave
    /// the queue one over the cap.
    steer_evicting: BTreeMap<AgentId, BTreeSet<Lsn>>,
    /// `Steer` records persisted but not yet folded, per agent.
    ///
    /// The other half of the same window: the depth `on_command` reads is the acknowledged one,
    /// so notes already on their way to the disk have to be counted or a burst that starts below
    /// the cap lands above it.
    steer_appending: BTreeMap<AgentId, usize>,
    /// The last liveness answer for each open turn, and the reaper's record of which turns it
    /// owns.
    ///
    /// A turn is *adopted* exactly when there is an entry here — a live-path child of this run is
    /// never probed, so it never gets one — which is why `Child` carries no `adopted` flag: two
    /// representations of one fact are two things to keep in step.
    ///
    /// Empty after a restart, deliberately: the age of an `Unknown` is
    /// diagnostic, not a decision, and the next run asks again rather than inheriting an answer
    /// about a process table it has not read.
    liveness: BTreeMap<TurnId, LivenessState>,
    /// Per-agent compaction deadline and suppressed-beat count.
    ///
    /// Run-local beside [`Self::liveness`], not replayed. Losing it makes the next exempt signal
    /// count, which is the safe restart direction.
    compaction: BTreeMap<AgentId, Compaction>,
    /// Off unless the daemon serves the Floor, the only reader that needs recent samples.
    activity_sampling: bool,
    /// The newest unjournaled kind per agent. Only [`Self::flush_activity`] journals it.
    activity_pending: BTreeMap<AgentId, (Option<TurnId>, LiveActivityKind)>,
    activity_journaled: BTreeMap<AgentId, JournaledActivity>,
    /// **The level the tick re-reads.** The signal is level-triggered rather than edge-triggered
    /// because a beat is a fixed 30-second tick and [`Self::beat`] drains: one raise per turn
    /// against one descent per beat oscillates `Steer → Healthy → Steer` forever and never stands
    /// at `Constrain`, so gate 5 is never reached. Holding the observation here lets
    /// [`Self::on_tick`] ask the question again on every beat, and the condition clears itself
    /// the moment an operator sets a ceiling or a later session reports no API key.
    ///
    /// Run-local beside [`Self::compaction`] and journaled nowhere, so invariant 1 is not reached:
    /// it is in-flight state like [`Self::signals`], not a projection `dudes replay` rebuilds. A
    /// restart empties it and the next managed init re-establishes it — one turn late, which
    /// fails toward *not* raising and costs protection rather than a false stop.
    last_session_on_api_key: BTreeSet<AgentId>,
    /// Fixed in production. The testing feature may raise it by one to prove the ceiling, rather
    /// than grace expiry, is what makes the breaker reach its cap.
    compaction_max_suppressed_beats: u8,
    /// Which standing cause each turn already has a `KillDeferred` on the disk for.
    ///
    /// A deferred kill is retried on every `Tick` while the cause stands; this is the
    /// only thing between that retry and ~2,880 identical records a day at a 30-second beat. Keyed
    /// by the cause and not by the turn alone, so an escalation from a breaker `Stop` to a `Halt`
    /// is a second fact and gets a second record.
    ///
    /// **Its entry lives until the turn's end is *folded*, not until that end is accepted.** The
    /// retry it guards is driven from the reaper's sweep, and the sweep reads the world — which
    /// still calls the turn an open child for as long as its `TurnEnded` is in flight. So the
    /// guard is drained in [`Self::apply_entry`], on the acknowledged entry, exactly where
    /// [`Self::ending`] hands over; dropping it in [`Self::end_turn`] would spend it inside the
    /// crash window and let one standing cause be journaled once per beat.
    deferred: BTreeMap<TurnId, KillReason>,
    /// Turns whose `TurnEnded` this run has emitted, until the fold takes over.
    ///
    /// The run-local half of "already ending", handed over to [`TurnRow::has_ended`] the moment
    /// the entry is acknowledged. It exists because the world's half cannot cover the window
    /// between the two: `World::apply` moves the status on the *acknowledged* entry, so a
    /// `ChildExited` arriving after a reaper end was emitted and before it was fsynced would find
    /// a row that still reads `Spawned`.
    ending: BTreeSet<TurnId>,
    /// Breaker signals seen since each agent's last beat.
    ///
    /// **A set, not a list.** The ladder asks one question of a beat's signals — was any of them
    /// counting — so the multiplicity carries no decision, and a record that grew with every
    /// repeat would put an unbounded field in the journal for a value bounded at four. What the
    /// beat records is therefore *which kinds* were seen in the window; how many times a hook
    /// timed out inside one 30-second window is a metric, not a reason.
    signals: BTreeMap<AgentId, BTreeSet<BreakerSignal>>,
    clients: BTreeSet<ClientId>,
    /// This run's agent tokens. Run-local by construction: the plaintext is never
    /// journaled, so a restart starts empty and mints again, and every hash already journaled for
    /// a live agent stays valid (set membership, not recency).
    tokens: Mint,
    next_token: u64,
    pending: Vec<Effect>,
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Mint a board card id from the daemon's configured random source.
    pub fn mint_card_id(&mut self) -> CardId {
        CardId::from_ulid(self.rng.ulid())
    }

    /// Mint a mail conversation id from the daemon's configured random source.
    pub fn mint_thread_id(&mut self) -> ThreadId {
        ThreadId::from_ulid(self.rng.ulid())
    }

    /// A machine that has folded nothing.
    pub fn new(config: Config, clock: C, rng: R) -> Self {
        Self {
            config,
            adapters: Adapters::default(),
            clock,
            rng,
            world: World::default(),
            closing_active: false,
            closing_recovery: false,
            missing_worktrees: BTreeSet::new(),
            status: BTreeMap::new(),
            ledger: Ledger::new(),
            spend: BTreeMap::new(),
            mail_refreshes: BTreeMap::new(),
            next_mail_request: 0,
            work: BTreeMap::new(),
            goals_awaiting_board_result: BTreeSet::new(),
            children: BTreeMap::new(),
            mcp_attempts: BTreeMap::new(),
            kills: BTreeMap::new(),
            reissued: BTreeMap::new(),
            reissue_confirmations: BTreeMap::new(),
            spawning: BTreeSet::new(),
            browser_delivery_open: true,
            browser_launches: BTreeMap::new(),
            browser_generation: 0,
            browser_context: BTreeMap::new(),
            steering: BTreeMap::new(),
            steer_evicting: BTreeMap::new(),
            steer_appending: BTreeMap::new(),
            liveness: BTreeMap::new(),
            compaction: BTreeMap::new(),
            activity_sampling: false,
            activity_pending: BTreeMap::new(),
            activity_journaled: BTreeMap::new(),
            last_session_on_api_key: BTreeSet::new(),
            compaction_max_suppressed_beats: COMPACTION_MAX_SUPPRESSED_BEATS,
            deferred: BTreeMap::new(),
            ending: BTreeSet::new(),
            signals: BTreeMap::new(),
            clients: BTreeSet::new(),
            tokens: Mint::default(),
            next_token: 0,
            pending: Vec::new(),
        }
    }

    /// The registry this run launches from, built once from the operator's `[providers]` table.
    ///
    /// One call, at construction, and no way to add an adapter afterwards: the daemon's door
    /// reads [`Self::adapters`] to decide whether a hire is admissible, and a registry that could
    /// grow would let it answer differently depending on when it asked.
    ///
    /// **A second call replaces; it does not accumulate.** A reader used to an `insert` would
    /// expect two calls to leave two adapters and get one. There is one call site, and `#[must_use]`
    /// catches a result thrown away, so nothing today can hit it — but the asymmetry is exactly
    /// why the old name could not be kept as a delegating shim: same signature, opposite
    /// behaviour, and an accumulating caller would have silently ended up with only its last
    /// adapter.
    #[must_use]
    pub fn with_adapters(mut self, adapters: Adapters) -> Self {
        self.adapters = adapters;
        self
    }

    /// Name the daemon's `agent-bin` directory once it exists. Called before the first launch;
    /// the directory is a startup fact and not a journaled one.
    pub fn set_agent_bin(&mut self, dir: Option<PathBuf>) {
        self.config.agent_bin = dir;
    }

    /// Accept hook and program activity samples for [`Input::ActivityFlush`] to journal.
    ///
    /// The daemon calls this only when it serves the Floor. Without it no sample is kept and no
    /// `LiveActivityObserved` is written; lifecycle records still project their fixed kinds.
    pub fn enable_activity_sampling(&mut self) {
        self.activity_sampling = true;
    }

    /// How many agents hold a run-local compaction entry.
    ///
    /// The state is unjournalled, so a test has no other way to see that
    /// `on_hook` declined to allocate for an `AgentId` with no ladder.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn test_compaction_entries(&self) -> usize {
        self.compaction.len()
    }

    /// Override the compaction suppression ceiling in a test fixture.
    ///
    /// This is compiled only with the testing feature; production has no configuration or wire
    /// surface that can move the security parameter.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_test_compaction_max_suppressed_beats(&mut self, beats: u8) {
        self.compaction_max_suppressed_beats = beats;
    }

    /// Whether this run still owns the launch claim for `turn`.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn test_is_spawning(&self, turn: TurnId) -> bool {
        self.spawning.contains(&turn)
    }

    /// The registry, for the door that must refuse a hire this machine could never launch.
    #[must_use]
    pub const fn adapters(&self) -> &Adapters {
        &self.adapters
    }

    /// What this daemon knows that is not in the journal, for the door that must refuse a call
    /// setting no ceiling while the environment carries a billing credential a child would
    /// inherit.
    ///
    /// Every field is fixed at construction, so a reader needs no pump and cannot see a value a
    /// pending `Persist` would change — the same property that makes [`Supervisor::adapters`] safe
    /// to read from a refusing arm.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Move emitted requests to an executor that must take a final serialized dispatch claim.
    #[must_use]
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.pending)
    }

    /// Gate 7's whole retained carrier for `agent`, or its whole default when none was pushed.
    ///
    /// Callers that refresh one projection-owned field clone this value and send a replacement
    /// [`Input::Work`], preserving the other fields without making this crate own their sources.
    #[must_use]
    pub fn work_input(&self, agent: AgentId) -> WorkInput {
        self.work.get(&agent).cloned().unwrap_or_default()
    }

    /// Begin one closure only after existing durable launch continuations are resolved.
    ///
    /// # Errors
    /// Returns a typed refusal before persisting an announcement.
    pub fn closing_announce(&mut self) -> Result<&[Effect], crate::closing::ClosingError> {
        use crate::closing::ClosingError;
        self.pending.clear();
        if self.closing_active {
            return Ok(&self.pending);
        }
        if !self.ledger.is_empty()
            || !self.spawning.is_empty()
            || self.world.issued_not_spawned().next().is_some()
            || self.world.turns.iter().any(|(turn, row)| {
                !row.has_ended()
                    && self.world.browser_issue(*turn).is_some_and(|issue| {
                        matches!(
                            issue.cancellation,
                            Some(crate::world::BrowserIssueCancellation::Ambiguous { .. })
                        )
                    })
            })
            || self.closing_recovery
        {
            return Err(ClosingError::LaunchPending);
        }
        if self
            .world
            .open_children()
            .any(|(_, row)| row.adoption_key().is_none())
            || self.world.attached_agents().any(|agent| {
                !self
                    .world
                    .attached_children()
                    .any(|(owner, _)| owner == agent)
            })
        {
            return Err(ClosingError::OwnershipUnproven);
        }
        let participants = crate::closing::ClosingSession::participant_ids(&self.world);
        if participants.len() > dudes_proto::rpc::closing::CLOSING_PARTICIPANT_LIMIT {
            return Err(ClosingError::TooManyParticipants);
        }
        self.persist(
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Announced,
            },
            Then::ClosingAnnounced,
        );
        Ok(&self.pending)
    }

    /// Persist one authenticated own ACK, retaining current retirement precedence.
    ///
    /// # Errors
    /// Refuses a retired caller, wrong closure, nonparticipant or terminal unacknowledged closure.
    pub fn closing_ack(
        &mut self,
        announcement: Lsn,
        agent: AgentId,
    ) -> Result<&[Effect], crate::closing::ClosingError> {
        use crate::closing::ClosingError;
        self.pending.clear();
        if self.world.agents.get(&agent).is_none_or(|row| row.retired) {
            return Err(ClosingError::Retired);
        }
        let session = self
            .world
            .closing
            .as_ref()
            .filter(|session| session.announcement == announcement)
            .ok_or(ClosingError::WrongAnnouncement)?;
        let participant = session
            .participants
            .get(&agent)
            .ok_or(ClosingError::NotParticipant)?;
        if participant.acknowledged.is_some() {
            return Ok(&self.pending);
        }
        if session.is_complete() {
            return Err(ClosingError::NotReady);
        }
        if !self.ledger.is_empty() {
            return Err(ClosingError::LaunchPending);
        }
        self.persist(
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Ack { agent },
            },
            Then::Fold,
        );
        Ok(&self.pending)
    }

    /// Journal teardown eligibility after acknowledged Managed finish or observed expiry.
    ///
    /// # Errors
    /// Refuses before the ACK folds, before readiness without expiry, or without an active closure.
    pub fn closing_finish(
        &mut self,
        expired: bool,
    ) -> Result<&[Effect], crate::closing::ClosingError> {
        use crate::closing::ClosingError;
        self.pending.clear();
        if !self.ledger.is_empty() {
            return Err(ClosingError::LaunchPending);
        }
        let session = self
            .world
            .closing
            .as_ref()
            .filter(|_| self.closing_active)
            .ok_or(ClosingError::WrongAnnouncement)?;
        if session.is_complete() {
            return Ok(&self.pending);
        }
        if !expired && !session.ready(&self.world) {
            return Err(ClosingError::NotReady);
        }
        self.persist(
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Complete,
            },
            Then::Fold,
        );
        Ok(&self.pending)
    }

    /// Whether this daemon run has an active closing issuance fence.
    #[must_use]
    pub const fn is_closing(&self) -> bool {
        self.closing_active
    }

    /// Actual closing phase for this run, excluding historical completed closures.
    #[must_use]
    pub fn closing_phase(&self) -> Option<dudes_proto::ClosingPhase> {
        self.closing_active
            .then(|| self.world.closing.as_ref().map(|session| session.phase))
            .flatten()
    }

    /// Existing Floor phase without participant acknowledgement identity or counts.
    #[must_use]
    pub fn closing_floor_phase(&self) -> Option<dudes_proto::ClosingPhase> {
        self.closing_phase().map(crate::closing::floor_phase)
    }

    /// Reconcile only resources that belonged to a historical Complete before normal work.
    pub fn closing_recover(&mut self) -> &[Effect] {
        self.pending.clear();
        if !self.closing_recovery {
            return &self.pending;
        }
        let turns: Vec<_> = self
            .world
            .closing
            .as_ref()
            .expect("recovery closure")
            .managed_cleanup
            .iter()
            .copied()
            .collect();
        for turn in turns {
            if self.world.browser_issue_cancelled(turn) {
                continue;
            }
            let Some(row) = self.world.turns.get(&turn).cloned() else {
                continue;
            };
            if row.is_issued_not_spawned() {
                self.reissue_turn(turn, row.agent, &row.task, row.model_selection.clone());
            } else if let Some(key) = row.adoption_key() {
                if self
                    .liveness
                    .get(&turn)
                    .copied()
                    .is_some_and(LivenessState::is_unknown)
                {
                    if self.deferred.get(&turn) != Some(&KillReason::Shutdown) {
                        self.deferred.insert(turn, KillReason::Shutdown);
                        self.persist(
                            Record::KillDeferred {
                                agent: row.agent,
                                turn,
                                reason: dudes_proto::KillDeferredReason::LivenessUnknown,
                            },
                            Then::Fold,
                        );
                    }
                } else if self.children.contains_key(&turn) {
                    self.kills.insert(turn, KillReason::Shutdown);
                    self.pending.push(Effect::Kill {
                        agent: row.agent,
                        turn,
                        key: key.clone(),
                    });
                    self.pending.push(Effect::Probe { turn, key });
                } else {
                    self.pending.push(Effect::Probe { turn, key });
                }
            }
        }
        if self
            .world
            .closing
            .as_ref()
            .is_some_and(|session| !session.needs_recovery())
        {
            self.closing_recovery = false;
            let ready: Vec<_> = self
                .world
                .agents
                .iter()
                .filter_map(|(agent, row)| {
                    (!row.retired && !row.attached && row.open_turn.is_none()).then_some(*agent)
                })
                .collect();
            for agent in ready {
                self.decide(agent);
            }
        }
        &self.pending
    }

    /// Historical Attached identities still awaiting an observed close.
    pub fn closing_recovery_attachments(
        &self,
    ) -> impl Iterator<Item = (AgentId, AdoptionKey)> + '_ {
        self.world
            .attached_children()
            .filter(|(agent, _)| {
                self.world
                    .closing
                    .as_ref()
                    .is_some_and(|session| session.attached_cleanup.contains(agent))
            })
            .map(|(agent, key)| (agent, key.clone()))
    }

    /// Whether historical closing cleanup still fences normal issuance.
    #[must_use]
    pub const fn recovering_closing(&self) -> bool {
        self.closing_recovery
    }

    /// React to one input.
    ///
    /// The returned slice is valid until the next call: `handle` clears it on entry, so a caller
    /// consumes the effects before handing over the next input.
    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive typed input routing stays in one visible match"
    )]
    pub fn handle(&mut self, input: Input) -> &[Effect] {
        self.pending.clear();
        match input {
            Input::MissingWorktrees(agents) => self.set_missing_worktrees(agents),
            Input::Started => self.on_started(),
            Input::Journaled { lsn, entry } => self.mail_journaled(lsn, &entry),
            Input::MailWakeReady {
                agent,
                request,
                result,
            } => self.mail_wake_ready(agent, request, result),
            Input::Replayed { lsn, entry } => self.apply_entry(lsn, &entry),
            Input::Command(command) => self.on_command(command),
            Input::Hook { hook, event } => self.on_hook(hook, &event),
            Input::Persisted { token, lsn } => self.on_persisted(token, lsn),
            Input::Spawned {
                turn,
                key,
                mcp_grant,
            } => self.on_spawned(turn, &key, mcp_grant),
            Input::SpawnFailed { turn, reason } => self.on_spawn_failed(turn, reason),
            Input::BrowserLaunchReady {
                turn,
                generation,
                prepared,
            } => {
                self.on_browser_launch_ready(turn, generation, prepared);
            }
            Input::SessionLearned { turn, session } => self.on_session_learned(turn, &session),
            Input::ActivityObserved { turn, kind } => self.on_activity_observed(turn, kind),
            Input::ActivityFlush => self.flush_activity(),
            Input::ChildExited {
                final_reply,
                elapsed_ms,
                turn,
                exit,
                usage,
                model_usage,
                outcome,
                last_message,
            } => self.end_turn(
                turn,
                ManagedEnd {
                    elapsed_ms,
                    exit,
                    usage,
                    model_usage,
                    outcome,
                    last_message,
                    final_reply,
                },
            ),
            Input::PredicateDone {
                goal,
                pass,
                evidence,
            } => self.on_predicate_done(goal, pass, evidence),
            Input::PtyExited { agent } => {
                self.persist(
                    Record::Detached {
                        agent,
                        by: Actor::System,
                        provenance: None,
                    },
                    Then::Decide { agent },
                );
            }
            Input::Spend { agent, view } => {
                self.spend.insert(agent, view);
            }
            Input::Work { agent, view } => {
                self.work.insert(agent, view);
            }
            Input::SessionObserved {
                turn,
                key,
                mcp_grant,
                observation,
            } => {
                self.on_session_observed(turn, key, mcp_grant, observation);
            }
            Input::Signal { agent, signal } => self.on_signal(agent, signal),
            Input::Tick => self.on_tick(),
            Input::Liveness { turn, liveness } => self.on_liveness(turn, liveness),
            Input::PtySpawned { agent, pty, key } => self.persist(
                Record::PtySpawned {
                    agent,
                    pty,
                    pid: key.pid,
                    start_time: key.start_time,
                    boot: key.boot,
                },
                Then::Fold,
            ),
            Input::BoardChanged { lsn, entry } => {
                let card = match &entry.record {
                    Record::CardCreated { card, .. }
                    | Record::CardTransition { card, .. }
                    | Record::Claim { card, .. }
                    | Record::TicketCreated { card, .. }
                    | Record::TicketAssigned { card, .. }
                    | Record::TicketStarted { card, .. }
                    | Record::TicketRemoved { card, .. } => *card,
                    _ => return &self.pending,
                };
                self.apply_entry(lsn, &entry);
                let goals: Vec<_> = self.world.held_board_goals(Some(card)).collect();
                for (agent, goal) in goals {
                    self.judge(agent, goal);
                }
            }
        }
        &self.pending
    }

    /// Effects emitted by the most recent [`Self::handle`] call.
    ///
    /// This is the same buffer `handle` returns, exposed separately so a caller can end that
    /// method's mutable borrow, inspect the newly folded [`World`], then execute the effects
    /// without cloning or reordering them. The slice remains valid only until the next `handle`
    /// call, which clears and repopulates the buffer.
    #[doc(hidden)]
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.pending
    }

    /// The fold, for a caller that wants to read it (`system.doctor`, the tests).
    #[must_use]
    pub const fn world(&self) -> &World {
        &self.world
    }

    /// What the reaper last answered about one open turn, and the age of the current run of
    /// `Unknown` answers. A turn it has not asked about reports `(Unknown, None)` — a live-path
    /// child of this run is never probed, so it never gets an entry or a first-answer stamp.
    ///
    /// Crate-visible for `system.doctor`'s per-turn listing (`crate::doctor`), which reports the
    /// answer and never acts on it. Nothing outside this crate reads the map: an `Unknown` here is
    /// diagnostic, and no decision may be taken from it.
    pub(crate) fn liveness_of(&self, turn: TurnId) -> (Liveness, Option<u64>) {
        match self.liveness.get(&turn).copied() {
            None => (Liveness::Unknown, None),
            Some(LivenessState::Alive) => (Liveness::Alive, None),
            Some(LivenessState::Unknown { first_at }) => {
                let elapsed = self.clock.now().millis_since(first_at);
                (
                    Liveness::Unknown,
                    Some(u64::try_from(elapsed).unwrap_or_default()),
                )
            }
        }
    }

    /// Whether `agent` has an open turn whose last recorded liveness is exactly `Unknown`.
    ///
    /// This is the narrow pre-command read `agent.retire` is given. It exposes neither
    /// the liveness map nor a general decision surface: the caller can only ask the one question
    /// that decides whether retirement must be refused before it becomes durable.
    #[must_use]
    pub fn retirement_has_unknown_liveness(&self, agent: AgentId) -> bool {
        self.world
            .agents
            .get(&agent)
            .and_then(|row| row.open_turn)
            .and_then(|turn| self.liveness.get(&turn).copied())
            .is_some_and(LivenessState::is_unknown)
    }

    /// Where an agent is in its life.
    #[must_use]
    pub fn status_of(&self, agent: AgentId) -> AgentStatus {
        self.status
            .get(&agent)
            .copied()
            .unwrap_or(AgentStatus::Idle)
    }

    // ---- the journal side -------------------------------------------------------------------

    /// Stamp a record and emit the `Persist` that carries it, parking what its acknowledgement
    /// authorises.
    ///
    /// `at` is the value `Clock::now()` returned at the moment this effect was built. The run-local compaction deadline is the clock's other reader.
    fn persist(&mut self, record: Record, then: Then) {
        self.persist_at(self.clock.now(), record, then);
    }

    fn persist_at(&mut self, at: Timestamp, record: Record, then: Then) {
        let entry = Entry { at, record };
        self.next_token += 1;
        let token = PersistToken(self.next_token);
        self.ledger.insert(
            token,
            Parked {
                entry: entry.clone(),
                then,
            },
        );
        self.pending.push(Effect::Persist { token, entry });
    }

    /// Track the newest journaled activity and forget samples a lifecycle record supersedes.
    fn fold_activity(&mut self, entry: &Entry) {
        if let Some((agent, _)) = entry.record.goal_install() {
            self.activity_pending.remove(&agent);
            if let Some(journaled) = self.activity_journaled.get_mut(&agent) {
                journaled.kind = None;
            }
            return;
        }
        match &entry.record {
            Record::LiveActivityObserved { agent, kind } => {
                self.activity_journaled.insert(
                    *agent,
                    JournaledActivity {
                        kind: Some(*kind),
                        at: entry.at,
                    },
                );
            }
            Record::AgentRetired { id, .. } => {
                self.activity_pending.remove(id);
                self.activity_journaled.remove(id);
            }
            Record::AgentHeld { agent, .. }
            | Record::GoalCleared { agent, .. }
            | Record::TurnSpawned { agent, .. }
            | Record::TurnEnded { agent, .. }
            | Record::ConversationQuestion { agent, .. }
            | Record::ConversationAnswerQueued { agent, .. }
            | Record::HumanQuestion { agent, .. }
            | Record::HumanAnswer { agent, .. }
            | Record::Pause { agent, .. }
            | Record::Resume { agent, .. }
            | Record::Halt { agent, .. }
            | Record::Detached { agent, .. } => {
                self.activity_pending.remove(agent);
                if let Some(journaled) = self.activity_journaled.get_mut(agent) {
                    journaled.kind = None;
                }
            }
            _ => {}
        }
    }

    /// Fold one confirmed entry, appended at `lsn`, into the world and move the agent's status
    /// from it.
    ///
    /// Called by both `Input::Persisted` and `Input::Replayed`, which is the point: statuses are
    /// derived from records, so replaying the same records reaches the same statuses. Both
    /// callers hold the entry's position and pass it here for the same reason: a fold that saw
    /// one position live and another on replay would break exactly that equality.
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive match applies each durable lifecycle carrier"
    )]
    fn apply_entry(&mut self, lsn: Lsn, entry: &Entry) {
        self.world.apply(lsn, entry);
        self.fold_activity(entry);
        if let Record::PredicateResult { goal, .. } = &entry.record {
            self.goals_awaiting_board_result.remove(goal);
        }
        if entry.record.goal_install().is_some()
            || matches!(
                entry.record,
                Record::GoalCleared { .. }
                    | Record::GoalReassigned { .. }
                    | Record::AgentRetired { .. }
                    | Record::LoopDecided {
                        decision: LoopDecision::Done,
                        ..
                    }
            )
        {
            self.goals_awaiting_board_result.retain(|goal| {
                self.world
                    .held_board_goals(None)
                    .any(|(_, held)| held == *goal)
            });
        }
        match &entry.record {
            // `Continue` moves nothing here: the agent is `Running` when the turn exists, which
            // is the `TurnIssued` below. A crash between the two leaves an `Idle` agent with no
            // open turn, and `Started` decides again — correctly, because that decision was
            // never acted on.
            Record::LoopDecided {
                agent, decision, ..
            } => {
                let event = FsmEvent::of_decision(decision);
                if event != FsmEvent::Continued {
                    self.move_status(*agent, event);
                }
            }
            Record::TurnIssued { agent, .. }
            | Record::TurnIssuedWithBrowserNote { agent, .. }
            | Record::NoteOnlyTurnIssued { agent, .. }
            | Record::ConversationTurnIssued { agent, .. }
            | Record::TaughtNoteOnlyTurnIssued { agent, .. } => {
                self.move_status(*agent, FsmEvent::Continued);
            }
            Record::MailWakeOnlyTurnIssued(issue) if self.world.mail_wake_entry_accepted(lsn) => {
                self.move_status(issue.agent, FsmEvent::Continued);
            }
            Record::MailWakeTurnDecided(decision) if self.world.mail_wake_entry_accepted(lsn) => {
                let event = match decision.decision {
                    dudes_proto::MailWakeAdmission::Issue { .. } => FsmEvent::Continued,
                    dudes_proto::MailWakeAdmission::Wait { .. } => FsmEvent::Waited,
                    dudes_proto::MailWakeAdmission::Hold { .. } => FsmEvent::Held,
                    dudes_proto::MailWakeAdmission::Exhausted { .. } => FsmEvent::Exhausted,
                };
                if event != FsmEvent::Continued {
                    self.move_status(decision.agent, event);
                }
            }
            Record::ConversationTurnDecided {
                agent, decision, ..
            }
            | Record::NoteTurnDecided {
                agent, decision, ..
            }
            | Record::TaughtNoteTurnDecided {
                agent, decision, ..
            } => {
                let event = FsmEvent::of_note_admission(decision);
                if event != FsmEvent::Continued {
                    self.move_status(*agent, event);
                }
            }
            Record::BrowserNoteIssuedCancelled { agent, turn, .. } => {
                if self.world.browser_issue_cancelled(*turn) {
                    self.browser_launches.remove(turn);
                    self.spawning.remove(turn);
                    self.reissued.remove(turn);
                    self.reissue_confirmations.remove(turn);
                    self.status
                        .insert(*agent, self.world.settled_status(*agent));
                }
            }
            Record::BrowserNoteReissueCancelled { turn, .. } => {
                if self.world.browser_issue_cancelled(*turn) {
                    self.browser_launches.remove(turn);
                    self.spawning.remove(turn);
                    self.reissued.remove(turn);
                    self.reissue_confirmations.remove(turn);
                }
            }
            Record::BrowserNoteContextEmitted { agent, .. } => {
                self.browser_context.remove(agent);
            }
            Record::TurnSpawned { turn, .. } => {
                // The hand-over: `World::apply` now calls the turn `Spawned`, so crash rule (2)
                // can no longer see it as re-issuable and the run-local claim can be released.
                // Releasing at emit time leaves the world `Issued` until this acknowledgement;
                // a cause cleared inside that window would then emit a second launch.
                self.spawning.remove(turn);
                self.browser_launches.remove(turn);
            }
            Record::TurnEnded { agent, turn, .. } => {
                // The hand-over: from here on `TurnRow::has_ended` is what refuses a second end,
                // so the run-local set does not have to grow for the life of the daemon.
                self.ending.remove(turn);
                // The once-per-cause guard is drained **here**, at the durable hand-over. The
                // accepted end removes liveness and child ownership, and `on_liveness` refuses
                // every late answer while `ending` owns the turn, so nothing can act on or refill
                // the guard inside that window. Keeping the existing entry until the fold also
                // keeps a refused second end from spending it early; the fold then bounds the map.
                self.deferred.remove(turn);
                self.reissued.remove(turn);
                self.reissue_confirmations.remove(turn);
                self.browser_launches.remove(turn);
                self.move_status(*agent, FsmEvent::TurnEnded);
            }
            Record::AgentRetired { id, .. } => {
                self.compaction.remove(id);
                // The latch is bounded by the same record as the compaction entry: `on_tick`
                // skips a retired agent, so a value left here could never be read again and
                // would only grow the map for the life of the daemon.
                self.last_session_on_api_key.remove(id);
            }
            // The hand-over, the same shape as `TurnSpawned` above: the fold has removed the
            // note, so the claim that stood in for it while it was in flight is spent. Replay
            // takes these arms too and removes from empty maps, which is a no-op — run-local
            // state is rebuilt by the run, never by the journal.
            Record::SteerConsumed { agent, .. } => {
                self.steering.remove(agent);
            }
            Record::SteerCleared { agent, steer_lsn } => {
                if let Some(claimed) = self.steer_evicting.get_mut(agent) {
                    claimed.remove(steer_lsn);
                    if claimed.is_empty() {
                        self.steer_evicting.remove(agent);
                    }
                }
            }
            Record::Steer { agent, .. } => {
                if let Some(appending) = self.steer_appending.get_mut(agent) {
                    *appending = appending.saturating_sub(1);
                    if *appending == 0 {
                        self.steer_appending.remove(agent);
                    }
                }
            }
            Record::HumanAnswer { agent, .. } | Record::ConversationAnswerQueued { agent, .. } => {
                self.move_status(*agent, FsmEvent::Answered);
            }
            Record::Attached { agent, .. } => self.move_status(*agent, FsmEvent::Attached),
            Record::Detached { agent, .. } => self.move_status(*agent, FsmEvent::Detached),
            _ => {}
        }
    }

    fn move_status(&mut self, agent: AgentId, event: FsmEvent) {
        let status = if matches!(
            event,
            FsmEvent::TurnEnded | FsmEvent::Answered | FsmEvent::Detached
        ) {
            self.world.settled_status(agent)
        } else {
            agent_fsm::next(self.status_of(agent), event)
        };
        self.status.insert(agent, status);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive match resumes every fsynced continuation under its token"
    )]
    fn on_persisted(&mut self, token: PersistToken, lsn: Lsn) {
        let Some(parked) = self.ledger.remove(&token) else {
            return;
        };
        // The mint's plaintext becomes usable **here and not when it was drawn**: until this
        // acknowledgement the hash is not on the disk, and a child armed with an unjournaled
        // credential is a child nothing can resolve. This is the crate's
        // general rule for the one record whose "acting effect" is a launch — `effect.rs`: the
        // acting effect only on its acknowledgement.
        //
        // Deliberately not inside `apply_entry`: that function is the *fold*, run by
        // `Input::Replayed` as well, and a run-local plaintext is not a projection. Replay of a
        // previous run's `HookTokenMinted` must promote nothing, and here it cannot.
        //
        // The `token_hash` is what selects the draw, not the agent: several draws for one agent
        // can be outstanding at once (see `Mint::pending`), so promoting "the agent's pending
        // token" would arm a child with whichever draw came last — including one whose record is
        // still in flight.
        if let Record::HookTokenMinted { agent, token_hash } = &parked.entry.record {
            self.tokens.confirm(*agent, token_hash);
        }
        let reconcile = self.mail_event_recipients(&parked.entry.record);
        self.apply_entry(lsn, &parked.entry);
        match parked.then {
            Then::ConversationStartupReserved => self.on_started(),
            Then::ConversationRetryReserved { turn, generation } => {
                self.record_browser_dispatch(turn, generation);
            }
            Then::MailWakeContinue { agent } => self.request_mail_wake(agent),
            Then::MailWakeCheckIssue {
                agent,
                root_lsn,
                at,
                input_digest,
            } => self.request_mail_refresh(
                agent,
                MailRefreshPhase::CheckIssue {
                    root_lsn,
                    decision_lsn: lsn,
                    at,
                    input_digest,
                },
            ),
            Then::TaughtBudgetRefused { audit } => self.persist(*audit, Then::Fold),
            Then::ClosingAnnounced => self.closing_active = true,
            Then::ConversationIssue {
                agent,
                note_lsn,
                context_lsn,
                task,
            } => self.issue_conversation(agent, note_lsn, context_lsn, &task, lsn),
            Then::BrowserIssue {
                agent,
                task,
                answers,
                note_lsn,
                completion_lsn,
            } => {
                self.issue_turn(
                    agent,
                    &task,
                    answers,
                    lsn,
                    false,
                    Some((note_lsn, completion_lsn)),
                );
            }
            Then::BrowserDispatch { turn, generation } => {
                self.dispatch_browser_launch(turn, generation);
            }
            Then::CompletedCommandEnded { agent } | Then::NoteEnded { agent } => {
                self.completed_command_ended(agent);
            }
            Then::AnswerSaved { agent } => {
                self.answer_saved(agent);
                self.request_mail_wake(agent);
            }
            Then::ClosingIssue {
                agent,
                task,
                answers,
            } => self.issue_turn(agent, &task, answers, lsn, true, None),
            Then::Fold => {}
            Then::Issue {
                agent,
                task,
                answers,
            } => self.issue_turn(agent, &task, answers, lsn, false, None),
            Then::Spawn {
                agent,
                turn,
                launch,
            } => {
                self.spawn(agent, turn, launch);
            }
            Then::McpGranted {
                agent,
                turn,
                launch,
            } => {
                self.begin_mcp_attempt(turn, Some(lsn));
                self.push_spawn(agent, turn, launch, Some(lsn));
            }
            Then::SpawnArmed {
                agent,
                turn,
                launch,
                mcp_grant,
            } => self.push_spawn(agent, turn, launch, mcp_grant),
            Then::ReissueSpawned { agent, turn } => {
                self.persist_held_reissue_confirmation(agent, turn);
            }
            Then::Decide { agent } => self.decide(agent),
            Then::ClearedCause { agent } => self.on_cause_cleared(agent),
            Then::Question { agent, text } => {
                self.notify_question(agent, &text);
                self.decide(agent);
            }
            Then::Judge { agent, goal } => self.judge(agent, goal),
            Then::HoldAfterRapidFailureBeat {
                agent,
                first,
                latest,
            } => {
                if self.world.agents.get(&agent).is_some_and(|row| {
                    row.fast_failures == Some((first, latest)) && row.automatic_hold.is_none()
                }) {
                    self.hold_agent(
                        agent,
                        Provenance::Inferred,
                        dudes_proto::AutomaticHold::RepeatedFastFailure { first, latest },
                    );
                } else {
                    self.decide(agent);
                }
            }
            Then::OpenPty {
                agent,
                spec,
                install,
                cols,
                rows,
            } => self.push_open_pty(agent, spec, install, cols, rows),
            Then::IsolationLeak { agent, stop } => {
                self.notify_floor_resync();
                if stop {
                    self.kill_standing_cause(agent);
                }
            }
            Then::KillStandingCause { agent } => self.kill_standing_cause(agent),
            Then::Retired { agent } => {
                self.kill_standing_cause(agent);
                let goals: Vec<_> = self.world.held_board_goals(None).collect();
                for (agent, goal) in goals {
                    self.judge(agent, goal);
                }
            }
            Then::Reply { hook, reply } => self.pending.push(Effect::Reply { hook, reply }),
        }
        for agent in reconcile {
            self.request_mail_wake(agent);
        }
    }

    // ---- start ------------------------------------------------------------------------------

    fn set_missing_worktrees(&mut self, agents: Vec<AgentId>) {
        self.missing_worktrees = agents.into_iter().collect();
    }

    /// Hold each agent the daemon reported a missing worktree for, once, at start. The set is
    /// drained, so an agent that is resumed after its tree is restored is not held again.
    fn hold_missing_worktrees(&mut self) {
        for agent in std::mem::take(&mut self.missing_worktrees) {
            let Some(row) = self.world.agents.get(&agent) else {
                continue;
            };
            if row.retired || row.automatic_hold.is_some() || self.protection_pending(agent) {
                continue;
            }
            if let Some(worktree) = row.worktree {
                self.hold_agent(
                    agent,
                    Provenance::Observed,
                    dudes_proto::AutomaticHold::WorktreeMissing { worktree },
                );
            }
        }
    }

    /// The three crash rules, in order, then the first decisions of the run.
    fn on_started(&mut self) {
        self.hold_missing_worktrees();
        if !self.world.mail_wake_history_valid() {
            return;
        }
        let unresolved = self.world.issued_not_spawned().find_map(|(turn, row)| {
            let issue = self.world.browser_issue(turn)?;
            let intent = *issue.dispatch_intents.last()?;
            (issue.goal.is_none()
                && issue.spawn_lsn.is_none()
                && issue.end_lsn.is_none()
                && !issue.uncertain_dispatches.contains(&intent))
            .then_some((row.agent, turn, intent))
        });
        if let Some((agent, turn, dispatch_intent_lsn)) = unresolved {
            self.persist(
                Record::ConversationDispatchUncertain {
                    agent,
                    turn,
                    dispatch_intent_lsn,
                    charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
                },
                Then::ConversationStartupReserved,
            );
            return;
        }

        self.closing_active = self
            .world
            .closing
            .as_ref()
            .is_some_and(|session| !session.is_complete());
        self.closing_recovery = self
            .world
            .closing
            .as_ref()
            .is_some_and(|session| session.is_complete() && session.needs_recovery());
        let agents: Vec<_> = self.world.agents.keys().copied().collect();
        for agent in agents {
            self.protect_failures(agent);
        }
        // (1) A replayed `Attached` has no recorded PTY handle or independently confirmed old
        //     child absence. Detach-before-close is superseded, so leave it attached and
        //     let the ready filter below hold it. An empty *new* host is not `AlreadyClosed` for
        //     the old run's PTY. No lifecycle record or host effect is synthesized at startup.

        // (2) A `TurnIssued` without a `TurnSpawned`: re-issue it **once**, under the same
        //     `TurnId`, with no second `TurnIssued` — the record is already fsynced, so invariant
        //     2 is already satisfied for this turn. `reissued: true` goes on the `TurnSpawned`,
        //     which is why the flag lives there and not on `TurnIssued`.
        //
        //     **Filtered by standing cause, and here is why.** A journal holding `TurnIssued`
        //     then `Halt` describes an agent a human stopped. Re-issuing regardless would launch
        //     a real vendor process and kill it a moment later — `on_spawned` parks another
        //     `KillStandingCause` — but only after the child exists and can act on the working
        //     tree. Invariant 5's gate order is control-first, and a halt that a restart can
        //     briefly undo is not a halt. So a turn whose agent is `halted` or `retired` — the
        //     two causes `kill_standing_cause` acts on — is not re-issued, and `Pause` is
        //     deliberately not among them because it kills no running turn either.
        //
        //     **The consequence, stated rather than discovered:** such a turn stays `Issued` with
        //     `open_turn` set, so the agent decides nothing for as long as the cause stands. That
        //     is the stuck-but-visible shape chosen over a wrong action, and
        //     `system.doctor` is what shows it.
        //
        //     **`Started` is not the only place the rule runs.** `loop.resume` re-runs it for the
        //     agent whose cause it lifted, through `Then::ClearedCause` and this same
        //     `reissue_turn` — so the stuck state ends when the halt does rather than at the next
        //     start. Both callers route through one function, which is what lets the exactly-once
        //     spawn guard be one line: a re-issue here and a re-issue from a cleared cause can
        //     otherwise reach the same `TurnId` while a launch for it is still outstanding.
        let reissue: Vec<(TurnId, AgentId, String, Option<ModelSelection>)> = self
            .world
            .issued_not_spawned()
            .map(|(turn, row)| {
                (
                    turn,
                    row.agent,
                    row.task.clone(),
                    row.model_selection.clone(),
                )
            })
            .collect();
        for (turn, agent, task, selection) in reissue {
            // What the turn recorded is not what it re-announces: a re-issue chooses its session
            // under the same logic a first issue does, and only a vendor-confirmed session is
            // resumable. The model is the other way round — the recorded selection
            // is replayed exactly — which is why the two travel separately. The
            // standing-cause filter described above is applied inside `reissue_turn`, not here, so
            // every re-issue path inherits it.
            self.reissue_turn(turn, agent, &task, selection);
        }

        // (3) A `TurnSpawned` without a `TurnEnded`: ask the process table about it, and let the
        //     answer decide. Nothing is written here — a probe reads and
        //     changes nothing, which is the one effect the machine emits without a record in
        //     front of it. A turn whose key cannot be rebuilt is skipped rather than guessed
        //     about: `adoption_key` returning `None` means `TurnSpawned` recorded no identifier,
        //     and there is nothing to ask about.
        let probes: Vec<Effect> = self
            .world
            .open_children()
            .filter_map(|(turn, row)| row.adoption_key().map(|key| Effect::Probe { turn, key }))
            .collect();
        self.pending.extend(probes);

        let board_goals: Vec<_> = self.world.held_board_goals(None).collect();
        for (agent, goal) in board_goals {
            self.judge(agent, goal);
        }

        // Then decide for every agent that can be decided for. A replayed attachment without an
        // observed close remains attached and is skipped, rather than resuming managed work.
        let ready: Vec<AgentId> = self
            .world
            .agents
            .iter()
            .filter(|(_, row)| !row.retired && !row.attached && row.open_turn.is_none())
            .map(|(agent, _)| *agent)
            .collect();
        for agent in ready {
            if self.reissue_pending(agent) {
                continue;
            }
            self.decide(agent);
        }
    }

    /// Re-issue one turn: refuse it while a standing cause forbids launching, announce under
    /// [`Self::announce_session`], remember the announced id run-locally, and park the `Spawn`.
    ///
    /// **Every re-issue routes through here**, not only crash rule (2)'s — and both halves of the
    /// rule live inside for the same reason. A path that forgets the announcement launches on a
    /// fresh session; a path that forgets the standing-cause filter launches a real vendor
    /// process inside a halted agent's working tree and kills it a moment later, which is what
    /// invariant 5's control-first gate order refuses (the why is spelled out at the second crash rule).
    /// Taking an [`AgentId`] and looking the row up here
    /// also deletes the `&AgentRow` parameter that a caller could pair with the wrong id.
    ///
    /// **The model is replayed, not re-chosen**. `recorded` is what the turn's own
    /// `TurnIssued` carries, and a `Some` is used verbatim even where the adapter's configuration
    /// has moved on since — the journal is the authority on what this turn was issued as. `None`
    /// is a journal written before the field: nothing in it can reconstruct the historical choice,
    /// so current configuration is the only answer available and re-issue keeps the earlier
    /// behaviour for exactly that case.
    fn reissue_turn(
        &mut self,
        turn: TurnId,
        agent: AgentId,
        task: &str,
        recorded: Option<ModelSelection>,
    ) {
        if !self.world.mail_wake_history_valid() {
            return;
        }
        if self.world.browser_issue(turn).is_some() && !self.browser_launch_allowed(agent, turn) {
            return;
        }
        if (self.closing_active || self.closing_recovery)
            && self.world.closing.as_ref().is_none_or(|session| {
                session
                    .participants
                    .get(&agent)
                    .is_none_or(|row| row.saving_turn != Some(turn))
            })
        {
            return;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if row.retired || row.halted || self.launch_held(agent) {
            return;
        }
        let session = self.announce_session(agent, true);
        let Some(selection) = recorded.or_else(|| self.model_selection(&row)) else {
            return;
        };
        match self.launch(agent, &row, task, &session, &selection) {
            ManagedLaunch::Ready(launch) => {
                if self.spawn(agent, turn, launch) {
                    // Only a re-issue this run actually took claims the announced id. `spawn()`
                    // answers the claim and not the emission — a launch parked under a mint is
                    // `true` and lands on this session once the mint is acknowledged.
                    self.reissued.insert(turn, session.id().cloned());
                }
            }
            ManagedLaunch::Refused(error) => self.end_launch_refused(turn, &error),
            ManagedLaunch::Unavailable => {}
        }
    }

    /// Take the run-local scheduling claim, the only insertion into [`Self::spawning`].
    /// Browser dispatch takes its separate executor claim immediately before the host call.
    ///
    /// Both paths that launch a turn come through here — the live one on the `Persisted` branch
    /// for the token that carried `TurnIssued`, and every re-issue — so invariant 2's
    /// exactly-once rule is one line in one place rather than a rule two handlers are each
    /// expected to remember. Deleting the guard breaks both callers' tests, not one.
    ///
    /// The rule it enforces is narrow and worth stating exactly: **not** "this turn was never
    /// launched", which the journal already answers, but "this *run* has a launch outstanding for
    /// this turn". Those differ only inside the window between `Effect::Spawn` and the
    /// `TurnSpawned` acknowledgement — and that window is precisely where a cleared standing
    /// cause can re-run crash rule (2) over a turn the fold still calls `Issued`.
    ///
    /// **The claim is here; the push is in [`Self::push_spawn`], and `true` does not mean an
    /// `Effect::Spawn` was emitted in this batch.** It means this run has taken the turn, which is
    /// the fact `reissue_turn` needs before claiming the session id it announced. When the agent
    /// has no acknowledged token yet, what goes out is the mint instead and the launch is parked
    /// under it. The window that opens is the one already documented on
    /// [`Self::spawning`]: a mint the disk never confirms leaves its turn claimed for the rest of
    /// the run, and the next start re-issues it once.
    fn spawn(&mut self, agent: AgentId, turn: TurnId, mut launch: Box<PreparedLaunch>) -> bool {
        if !self.world.mail_wake_history_valid() {
            return false;
        }
        if !self.spawning.insert(turn) {
            return false;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            self.spawning.remove(&turn);
            return false;
        };
        if launch.policy_lsn != row.mcp.revision {
            launch = match self.launch(
                agent,
                &row,
                &launch.task,
                &launch.session,
                &launch.selection,
            ) {
                ManagedLaunch::Ready(current) => current,
                ManagedLaunch::Refused(error) => {
                    self.spawning.remove(&turn);
                    self.end_launch_refused(turn, &error);
                    return false;
                }
                ManagedLaunch::Unavailable => {
                    self.spawning.remove(&turn);
                    return false;
                }
            };
        }
        if row.mcp.first_edit.is_some() {
            self.persist(
                Record::McpLaunchGranted {
                    agent,
                    turn,
                    policy_lsn: row.mcp.revision,
                    mcp_servers: row.mcp.desired,
                },
                Then::McpGranted {
                    agent,
                    turn,
                    launch,
                },
            );
        } else {
            self.begin_mcp_attempt(turn, None);
            self.push_spawn(agent, turn, launch, None);
        }
        true
    }

    fn begin_mcp_attempt(&mut self, turn: TurnId, grant: Option<Lsn>) {
        self.mcp_attempts.insert(
            turn,
            McpAttempt {
                grant,
                key: None,
                observations: Vec::new(),
            },
        );
    }

    // ---- arming a child ------------------------------------------------------------

    /// Arm a child, re-entered by [`Then::SpawnArmed`] once the mint is on the disk.
    /// Browser work then yields to preparation before its durable dispatch intent.
    ///
    /// Takes the [`AgentId`] rather than reading it off the turn row: a fallible lookup on this
    /// path would have a `None` arm, and the only thing that arm could do is launch a child with
    /// no credential or drop the launch silently. Both callers already hold the id.
    fn push_spawn(
        &mut self,
        agent: AgentId,
        turn: TurnId,
        mut launch: Box<PreparedLaunch>,
        mcp_grant: Option<Lsn>,
    ) {
        if !self.world.mail_wake_history_valid() {
            return;
        }
        let browser = self.world.browser_issue(turn).is_some();
        if browser && !self.browser_launch_allowed(agent, turn) {
            self.spawning.remove(&turn);
            self.reissued.remove(&turn);
            return;
        }
        if self.launch_held(agent) {
            self.spawning.remove(&turn);
            self.reissued.remove(&turn);
            return;
        }
        if self.compose_pair(agent, &mut launch.spec) {
            if browser {
                self.prepare_browser_launch(turn, launch.spec, launch.install, mcp_grant);
            } else {
                self.emit_spawn(turn, launch.spec, launch.install, mcp_grant);
            }
        } else {
            self.mint(
                agent,
                Then::SpawnArmed {
                    agent,
                    turn,
                    launch,
                    mcp_grant,
                },
            );
        }
    }

    fn emit_spawn(
        &mut self,
        turn: TurnId,
        spec: LaunchSpec,
        install: ManagedInstall,
        mcp_grant: Option<Lsn>,
    ) {
        self.pending.push(Effect::Spawn {
            turn,
            spec,
            install,
            mcp_grant,
        });
    }

    /// **The only constructor of [`Effect::OpenPty`] in this crate**, re-entered by
    /// [`Then::OpenPty`] once the mint is on the disk.
    ///
    /// An attached child is armed exactly as a managed one is — one token rule for both,
    /// "managed and attached alike" — and it counts `Effect::OpenPty` as a first spawn. What
    /// differs is only that `agent.attach` **reuses** the agent's token when there is one, which
    /// falls out of asking [`Self::compose_pair`] rather than being a second rule.
    fn push_open_pty(
        &mut self,
        agent: AgentId,
        mut spec: Box<LaunchSpec>,
        install: HookInstall,
        cols: u16,
        rows: u16,
    ) {
        if self.compose_pair(agent, &mut spec) {
            self.pending.push(Effect::OpenPty {
                agent,
                spec: *spec,
                install,
                cols,
                rows,
            });
        } else {
            self.mint(
                agent,
                Then::OpenPty {
                    agent,
                    spec,
                    install,
                    cols,
                    rows,
                },
            );
        }
    }

    /// Compose an attached launch only from replayed agent state and provider-owned typed parts.
    fn attach(
        &mut self,
        agent: AgentId,
        by: Actor,
        provenance: Option<dudes_proto::PeerProvenance>,
        cols: u16,
        rows: u16,
    ) {
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if row.retired || row.attached || row.open_turn.is_some() {
            return;
        }
        let Some(hook_command) = self
            .config
            .hook_command
            .as_deref()
            .filter(|path| path.is_absolute() && path.to_str().is_some())
        else {
            return;
        };
        let Some(session) = row.session.clone() else {
            return;
        };
        let Some(adapter) = self.adapters.get(row.provider) else {
            return;
        };
        let agent_dir = self.agent_dir(agent);
        let ctx = AgentCtx {
            agent,
            cwd: &row.cwd,
            agent_dir: &agent_dir,
            daemon_env: &self.config.daemon_env,
            sandbox: row.sandbox,
            trusted: row.trusted,
            max_turns: row.max_turns,
            max_budget: row.max_budget,
            mcp_servers: &[],
        };
        let Some(spec) = adapter.attached(&ctx, &TurnSession::Resume(session)) else {
            return;
        };
        // From the same context as the argv, so the `--settings` path it names is the file written.
        let install = adapter.install_hooks(&ctx, hook_command);
        self.persist(
            Record::Attached {
                agent,
                by,
                hook_port: None,
                provenance,
            },
            Then::OpenPty {
                agent,
                spec: Box::new(spec),
                install,
                cols,
                rows,
            },
        );
    }

    /// Append `DUDES_SOCK` and `DUDES_TOKEN` to a child's environment, when this run has an
    /// acknowledged token for `agent`. Answers whether it did.
    ///
    /// **`false` is an instruction, not a failure:** the caller must emit [`Self::mint`]'s record
    /// and park its launch under it, and must push no launching effect in this batch. `spec` is
    /// left untouched in that case, so the parked copy is armed exactly once, on re-entry.
    ///
    /// **This is the only place a token's plaintext leaves the mint as a value**,
    /// and the only place the pair is composed — which is what makes "the two travel together"
    /// structural: one function appends both or neither. They go in `env` and never in `argv`,
    /// because argv is world-readable.
    ///
    /// The workspace's one other non-test `HookToken::expose` call site is `Mint::draw`, which
    /// exposes a token only to hash it and answers the digest rather than the bytes. Two sites,
    /// not one: the claim worth making is that the plaintext reaches exactly one destination, a
    /// child's environment.
    ///
    /// Takes `&self`, so no path through here can change the machine's state as a side effect of
    /// reading a token.
    fn compose_pair(&self, agent: AgentId, spec: &mut LaunchSpec) -> bool {
        let Some(token) = self.tokens.live(agent) else {
            return false;
        };
        spec.env.push(EnvVar::new(
            "DUDES_SOCK",
            self.config.socket.as_os_str().to_owned(),
        ));
        spec.env.push(EnvVar::new("DUDES_TOKEN", token.expose()));
        spec.env.retain(|var| {
            let name = var.name().as_encoded_bytes();
            if cfg!(windows) {
                !name.eq_ignore_ascii_case(b"DUDES_MAILBOX")
            } else {
                name != b"DUDES_MAILBOX"
            }
        });
        spec.env.push(EnvVar::new(
            "DUDES_MAILBOX",
            self.config
                .data_dir
                .join("agents")
                .join(agent.to_string())
                .into_os_string(),
        ));
        true
    }

    /// Draw this agent's token for the run and emit the `Persist{HookTokenMinted}` carrying its
    /// **hash**, parking under it the launch that record arms.
    ///
    /// The plaintext goes nowhere near the record: `token_hash` is a `String` and no `Record`
    /// variant can hold a [`HookToken`] at all. The plaintext stays in [`Self::tokens`] for the
    /// rest of the run and dies with it, which is why the next run mints again and why every hash
    /// journaled for a live agent stays valid.
    fn mint(&mut self, agent: AgentId, then: Then) {
        let token_hash = self.tokens.draw(agent, &mut self.rng);
        self.persist(Record::HookTokenMinted { agent, token_hash }, then);
    }

    /// A standing cause was lifted: re-run crash rule (2) for this agent alone, then decide.
    ///
    /// The rule itself is not restated here — `reissue_turn` holds both halves of it, including
    /// the standing-cause filter, so an agent whose *other* cause still stands re-issues nothing.
    fn on_cause_cleared(&mut self, agent: AgentId) {
        self.retry_refused_browser_replays(agent);
        let reissue: Vec<(TurnId, String, Option<ModelSelection>)> = self
            .world
            .issued_not_spawned()
            .filter(|(_, row)| row.agent == agent)
            .map(|(turn, row)| (turn, row.task.clone(), row.model_selection.clone()))
            .collect();
        for (turn, task, selection) in reissue {
            self.reissue_turn(turn, agent, &task, selection);
        }
        self.decide(agent);
    }

    /// **It takes the agent rather than a session, and that is the point.** The only resumable id
    /// is the vendor-confirmed one on [`AgentRow::session`]; reading that field here instead of
    /// accepting a session makes `TurnRow.session` — the announced-but-unconfirmed id, which is
    /// what the superseded re-issue path read — **unreachable through this function**: no caller
    /// of it can reintroduce the defect by passing the wrong field.
    ///
    /// Unreachable here is narrower than unrepresentable, and the difference is worth stating.
    /// [`TurnSession`] is public and [`Self::launch`] takes one, so a future path that bypasses
    /// this function can still write `Resume(<an unconfirmed id>)` and hand it to `launch` — a
    /// plausible-looking line, not a visible omission. The seam stays where it is because pushing
    /// the choice into `launch` would cost `launch` the `&self` purity its own doc makes
    /// load-bearing. So: announce through here, and do not "simplify" the parameter back to a
    /// session.
    fn announce_session(&mut self, agent: AgentId, reissued: bool) -> TurnSession {
        let row = self.world.agents.get(&agent);
        if let Some(confirmed) = row
            .filter(|_| !reissued)
            .and_then(|row| row.session.clone())
        {
            return TurnSession::Resume(confirmed);
        }
        let pins = row
            .and_then(|row| self.adapters.get(row.provider))
            .is_some_and(|adapter| adapter.session_acquisition().pins());
        if pins {
            TurnSession::First(session_id_from_ulid(self.rng.ulid()))
        } else {
            TurnSession::Assigned
        }
    }

    fn reissue_pending(&self, agent: AgentId) -> bool {
        self.reissued.keys().any(|turn| {
            self.world.turns.get(turn).is_some_and(|row| {
                row.agent == agent
                    && row.is_issued_not_spawned()
                    && !self.world.browser_issue_cancelled(*turn)
            })
        })
    }

    // ---- the loop ---------------------------------------------------------------------------

    /// The one place a `LoopInput` is built, for `decide()` and for the tick's re-examination
    /// alike.
    ///
    /// **Both callers must build it here and nowhere else.** The sweep decides whether to decide by
    /// comparing this input's digest against the digest on the agent's last `LoopDecided`; a second
    /// construction site that drifted by one field would compare a digest against a *different
    /// input's* digest, and the answer would be wrong in a direction no test catches — the sweep
    /// would go on refusing to re-decide, which looks exactly like the defect it was written to fix.
    /// One function, so the two can only drift together.
    ///
    /// `None` for an agent the loop cannot decide for at all: absent, retired, or without a live
    /// goal. That is [`World::loop_input`]'s rule and this adds nothing to it.
    fn loop_input_for(&self, agent: AgentId) -> Option<LoopInput> {
        let spend = self.spend.get(&agent).copied().unwrap_or_default();
        // Cloned rather than copied, and the default is the whole of gate 7 going quiet: no card,
        // no mail, no digest, which `decide()` reads as the goal text alone. An agent the caller
        // has never pushed work for is therefore indistinguishable from one with nothing to do —
        // the same trade `spend` makes one line above, where the un-pushed agent looks unspent.
        let work = self.work.get(&agent).cloned().unwrap_or_default();
        self.world.loop_input(
            agent,
            spend,
            work,
            self.config.floor_usd_per_day,
            self.config.per_turn_floor,
        )
    }

    /// Assemble the input, ask `dudes_policy::decide()`, journal the verdict.
    ///
    /// Nothing decides while a turn is in flight: the loop is one turn deep per agent, and a
    /// second `Continue` would be a second process on one vendor session.
    fn decide(&mut self, agent: AgentId) {
        if !self.world.mail_wake_history_valid() {
            return;
        }
        if self.closing_active {
            self.decide_saving(agent);
            return;
        }
        if self.closing_recovery {
            return;
        }
        if self.protection_pending(agent) {
            return;
        }
        if self
            .world
            .agents
            .get(&agent)
            .is_some_and(|row| row.open_turn.is_some())
        {
            return;
        }
        let goal = self.world.agents.get(&agent).and_then(|row| row.goal);
        if goal.is_some_and(|goal| self.goals_awaiting_board_result.contains(&goal)) {
            return;
        }
        let Some(input) = self.loop_input_for(agent) else {
            self.decide_conversation(agent);
            self.decide_completed_note(agent);
            self.request_mail_wake(agent);
            return;
        };
        let browser_note = self.eligible_ordinary_note(agent);
        let Decided {
            decision,
            mut gate_trace,
            answers,
        } = match &browser_note {
            Some(note) => dudes_policy::decide_with_browser_note(&input, note),
            None => dudes_policy::decide(&input),
        };
        if let Some(cause) = self
            .world
            .agents
            .get(&agent)
            .and_then(|row| row.automatic_hold.as_ref())
            && decision
                == (LoopDecision::Hold {
                    reason: cause.hold_reason(),
                })
            && let Some(step) = gate_trace.last_mut()
        {
            step.note = Some(cause.to_string());
        }
        // Invariant 4's refusal, journaled before the verdict it produced: the admission check
        // runs before the turn is issued, so its record precedes the `LoopDecided` that carries
        // what the loop then did. An `Allowed` writes nothing.
        if let Some(refusal) = budget_refusal(agent, goal, &input, &gate_trace) {
            self.persist(refusal, Then::Fold);
        }
        let then = match &decision {
            LoopDecision::Continue { task } => match &browser_note {
                Some(note) => Then::BrowserIssue {
                    agent,
                    task: task.clone(),
                    answers,
                    note_lsn: note.note_lsn(),
                    completion_lsn: None,
                },
                None => Then::Issue {
                    agent,
                    task: task.clone(),
                    answers,
                },
            },
            LoopDecision::Done if self.browser_delivery_open => Then::Decide { agent },
            _ => Then::Fold,
        };
        self.persist(
            Record::LoopDecided {
                agent,
                // An unproducible digest is journaled as the **empty string**, and the decision is
                // still journaled: a decision that happened is a record (invariant 1), and one
                // that reached the machine's own gates is not made less true by a serialiser.
                //
                // Empty rather than `blake3_hex(b"")`, which an `unwrap_or_default()` would
                // produce: that would put the harm
                // the digest comparison guards against — two different inputs digesting equal — onto the wire, where
                // `dudes why` would print a well-formed 64-hex digest that is a lie about which
                // input this decision was taken from. An empty string is not 64 hex, so a reader
                // can tell the two apart, and it needs no schema change: `input_digest` is a plain
                // `string` with no `pattern` and no `minLength` on all three surfaces that carry
                // it (`schemas/journal-record.schema.json`, and twice in `schemas/rpc.schema.json`).
                //
                // The guard reads this back through `AgentRow::last_input_digest` and never has to
                // interpret it: a `None` *computed* digest is already "changed", so an agent whose
                // input will not serialise re-decides on every beat rather than never — which is
                // the chosen direction, one record against one stuck agent.
                input_digest: input_digest(&input).unwrap_or_default(),
                decision,
                gate_trace,
            },
            then,
        );
    }

    fn decide_saving(&mut self, agent: AgentId) {
        let Some(session) = self
            .world
            .closing
            .as_ref()
            .filter(|session| !session.is_complete())
        else {
            return;
        };
        let Some(participant) = session.participants.get(&agent) else {
            return;
        };
        let Some(row) = self.world.agents.get(&agent) else {
            return;
        };
        if self.world.closing_standing(agent, participant) != crate::closing::Standing::Open
            || participant.mode != Mode::Managed
            || participant.acknowledged.is_some()
            || participant.saving_turn.is_some()
            || row.open_turn.is_some()
            || self.protection_pending(agent)
        {
            return;
        }
        let Some(goal) = row.goal else {
            return;
        };
        let Some(ordinary) = self.loop_input_for(agent) else {
            return;
        };
        let input = dudes_policy::closing_input(session.announcement, agent, goal, ordinary);
        let dudes_policy::ClosingDecided::Policy(decided) = dudes_policy::decide_closing(&input)
        else {
            return;
        };
        if let Some(refusal) =
            budget_refusal(agent, Some(goal), &input.ordinary, &decided.gate_trace)
        {
            self.persist(refusal, Then::Fold);
        }
        let then = match &decided.decision {
            LoopDecision::Continue { task } => Then::ClosingIssue {
                agent,
                task: task.clone(),
                answers: decided.answers,
            },
            _ => Then::Fold,
        };
        self.persist(
            Record::LoopDecided {
                agent,
                input_digest: crate::host::closing_input_digest(&input).unwrap_or_default(),
                decision: decided.decision,
                gate_trace: decided.gate_trace,
            },
            then,
        );
    }

    /// Journal the turn the decision at `lsn_of_decision` called for, parking its `Spawn`.
    ///
    /// `answers` are the ones `task` carries; the record lists them so that its `TurnSpawned`
    /// consumes exactly those, and a re-issue replays them with the task. A return
    /// before the persist leaves them pending for the next decision.
    fn issue_turn(
        &mut self,
        agent: AgentId,
        task: &str,
        answers: Vec<Lsn>,
        lsn_of_decision: Lsn,
        closing: bool,
        browser: Option<(Lsn, Option<Lsn>)>,
    ) {
        if (self.closing_active && !closing) || self.closing_recovery {
            return;
        }
        let Some(row) = self.world.agents.get(&agent).cloned() else {
            return;
        };
        if self.launch_held(agent) {
            return;
        }
        let Some(goal) = row.goal else {
            return;
        };
        if let Some((note_lsn, completion)) = browser
            && (!self.browser_delivery_open
                || row.retired
                || row.halted
                || row.paused
                || row.attached
                || self.world.browser_note(note_lsn).is_none_or(|note| {
                    note.agent != agent
                        || note.state != crate::world::BrowserNoteState::Queued
                        || note.destination
                            != (dudes_proto::BrowserNoteDestination::ManagedGoal { goal })
                })
                || completion
                    .is_some_and(|done| self.world.completed_goal(agent) != Some((goal, done))))
        {
            return;
        }
        let turn = TurnId::from_ulid(self.rng.ulid());
        let session = self.announce_session(agent, false);
        // One selection, obtained **before** the record that carries it and handed to the launch
        // rather than re-derived inside it. Asking the adapter twice would let the
        // journal and the launch material disagree about the same turn.
        let Some(selection) = self.model_selection(&row) else {
            return;
        };
        let launch = match self.launch(agent, &row, task, &session, &selection) {
            ManagedLaunch::Ready(launch) => launch,
            ManagedLaunch::Unavailable => return,
            ManagedLaunch::Refused(error) => {
                self.persist(
                    Record::AgentHeld {
                        agent,
                        provenance: Provenance::Observed,
                        cause: dudes_proto::AutomaticHold::LaunchRefused {
                            reason: error.to_string(),
                        },
                    },
                    Then::Decide { agent },
                );
                return;
            }
        };
        let record = match browser {
            Some((note_lsn, Some(completion_lsn))) => Record::TaughtNoteOnlyTurnIssued {
                agent,
                turn,
                goal,
                task: task.to_owned(),
                session: session.id().cloned(),
                mode: Mode::Managed,
                model_selection: selection,
                lsn_of_decision,
                answers,
                note_lsn,
                completion_lsn,
            },
            Some((note_lsn, None)) => Record::TurnIssuedWithBrowserNote {
                agent,
                turn,
                goal,
                task: task.to_owned(),
                session: session.id().cloned(),
                mode: Mode::Managed,
                model_selection: selection,
                lsn_of_decision,
                answers,
                note_lsn,
            },
            None => Record::TurnIssued {
                agent,
                turn,
                goal,
                task: task.to_owned(),
                session: session.id().cloned(),
                mode: Mode::Managed,
                model_selection: Some(selection),
                lsn_of_decision,
                answers,
            },
        };
        self.persist(
            record,
            Then::Spawn {
                agent,
                turn,
                launch,
            },
        );
    }

    /// The adapter's launch for one managed turn. Pure: two calls with equal inputs give equal
    /// bytes, so a re-issue's launch differs from the first only where its inputs differ — which,,
    /// is the session it announces.
    fn launch(
        &self,
        agent: AgentId,
        row: &AgentRow,
        task: &str,
        session: &TurnSession,
        selection: &ModelSelection,
    ) -> ManagedLaunch {
        let Some(adapter) = self.adapters.get(row.provider) else {
            return ManagedLaunch::Unavailable;
        };
        // The hire admitted these names against the definitions of its run. A restart may have
        // changed a definition, so every launch admits them again, and a refusal holds the agent
        // before any turn is issued rather than launching what the hire would refuse.
        let mcp_servers = match self.config.mcp.admit(
            &row.mcp.desired,
            Rules {
                support: adapter.mcp_support(),
                default_isolation: row.mcp.isolation.is_none(),
                trusted: row.trusted,
            },
        ) {
            Ok(servers) => servers,
            Err(refusal) => return ManagedLaunch::Refused(LaunchError::McpServer(refusal)),
        };
        let agent_dir = self.agent_dir(agent);
        let ctx = AgentCtx {
            agent,
            cwd: &row.cwd,
            agent_dir: &agent_dir,
            daemon_env: &self.config.daemon_env,
            sandbox: row.sandbox,
            trusted: row.trusted,
            // From the replayed row, never from `self.config`: both limits are recorded at the
            // hire so that a crash re-issue composes the same argv as the first launch even when
            // the operator's configuration changed in between. Reading configuration here would
            // make history depend on the current file, which invariant 1 forbids.
            max_turns: row.max_turns,
            max_budget: row.max_budget,
            mcp_servers: &mcp_servers,
        };
        let mut spec = match adapter.managed(&ctx, task, session, selection) {
            Ok(spec) => spec,
            Err(error) => return ManagedLaunch::Refused(error),
        };
        if let Some(dir) = &self.config.agent_bin {
            prepend_agent_bin(&mut spec.env, dir);
        }
        let mut install = adapter.managed_install(&ctx, selection);
        install.directories.insert(0, agent_dir);
        ManagedLaunch::Ready(Box::new(PreparedLaunch {
            spec,
            install,
            policy_lsn: row.mcp.revision,
            task: task.to_owned(),
            session: session.clone(),
            selection: selection.clone(),
        }))
    }

    /// What this agent's adapter would select right now. `None` only where the
    /// agent's provider has no adapter on this floor, which is the same condition that already
    /// stops [`Self::launch`].
    fn model_selection(&self, row: &AgentRow) -> Option<ModelSelection> {
        self.adapters
            .get(row.provider)
            .map(Adapter::model_selection)
    }

    fn agent_dir(&self, agent: AgentId) -> PathBuf {
        self.config.data_dir.join("agents").join(agent.to_string())
    }

    // ---- the turn ---------------------------------------------------------------------------

    /// The host answered a launch. **Refused when the turn's end is already decided**, which
    /// mirrors [`Self::end_turn`]'s guard because it is the same guard read from the other side.
    ///
    /// Nothing in this crate's contract orders [`Input::Spawned`] before [`Input::ChildExited`] —
    /// `Spawned` is an observation after the fact and nothing waits on it, and the reaper's `Gone`
    /// is a second end-path that waits on nothing either — so an answer can arrive for a turn that
    /// has already ended. Writing `TurnSpawned` for it puts a spawn *after* the end in the journal
    /// and moves the row backwards out of `Ended`, which hands it back to the reaper: the next
    /// start finds an open child whose pid is long gone and ends it a second time as `Vanished`,
    /// over a turn that completed cleanly. A turn has exactly one end.
    ///
    /// **The disjunction is not decoration.** `World::apply` moves the row from the *acknowledged*
    /// entry, so between an end being emitted and its fsync `row.has_ended()` is still false —
    /// the crash window [`Self::end_turn`]'s own doc names — and a guard that read the row alone
    /// would let the record out inside it. Both halves have an arm in
    /// `tests/reaper.rs::a_late_spawned_does_not_reopen_an_ended_turn`, and the second arm is the
    /// one that fails against the row-only shape.
    ///
    /// **A refused answer leaves every run-local map exactly as it found it**, [`Self::spawning`]
    /// included: dropping the turn from it here would answer a launch for a turn the fold may
    /// still call `Issued`, and spend invariant 2's run-local guard on a process the next cleared
    /// cause would then start again.
    ///
    /// That is also why the refusal sits above [`Self::reissued`]'s lookup, and what it costs: a
    /// refused answer leaves one entry there until the turn ends. The lookup below fills
    /// `TurnSpawned.session` and the `reissued` flag; `on_session_learned` reads whether the same
    /// key is present through the pre-fold confirmation window. The other reader is `reissue_pending`, whose one
    /// caller is `on_started` — and
    /// [`Input::Started`] is "sent exactly once per daemon run", so a late answer arrives after
    /// the last read of the map by contract and not by handler ordering. The map is run-local, so
    /// the next start rebuilds it.
    fn on_spawned(&mut self, turn: TurnId, key: &AdoptionKey, mcp_grant: Option<Lsn>) {
        if self.browser_turn_cancelled(turn) {
            return;
        }
        let Some(row) = self.world.turns.get(&turn).cloned() else {
            return;
        };
        if self.ending.contains(&turn) || row.has_ended() {
            return;
        }
        if mcp_grant.is_some() && !self.mcp_attempts.contains_key(&turn) {
            return;
        }
        if self
            .mcp_attempts
            .get(&turn)
            .is_some_and(|attempt| attempt.grant != mcp_grant)
        {
            return;
        }
        if mcp_grant.is_none() && self.world.agents[&row.agent].mcp.first_edit.is_some() {
            return;
        }
        let pending = self.mcp_attempts.entry(turn).or_insert_with(|| McpAttempt {
            grant: mcp_grant,
            key: None,
            observations: Vec::new(),
        });
        if pending
            .key
            .as_ref()
            .is_some_and(|old| !same_child_key(old, key))
        {
            return;
        }
        pending.key = Some(key.clone());
        let observations = std::mem::take(&mut pending.observations);
        // The id the launch announced, which for a re-issue is not the one the projection holds.
        let announced = self.reissued.get(&turn).cloned();
        let reissued = announced.is_some();
        self.children.insert(turn, Child { key: key.clone() });
        let then = if reissued {
            Then::ReissueSpawned {
                agent: row.agent,
                turn,
            }
        } else {
            Then::KillStandingCause { agent: row.agent }
        };
        self.persist(
            Record::TurnSpawned {
                agent: row.agent,
                turn,
                pid: key.pid,
                start_time: key.start_time,
                boot: key.boot.clone(),
                session: announced.unwrap_or(row.session),
                reissued,
                mcp_grant,
            },
            then,
        );
        for (observed_key, observation) in observations {
            self.on_session_observed(turn, observed_key, mcp_grant, observation);
        }
    }

    /// The host confirmed this attempt left no process. Earlier browser dispatch uncertainty
    /// remains open; only a first attempt can take the existing failed-end path.
    fn on_spawn_failed(&mut self, turn: TurnId, reason: String) {
        if self.browser_turn_cancelled(turn) {
            return;
        }
        if self.retain_failed_browser_replay(turn) {
            self.mcp_attempts.remove(&turn);
            return;
        }
        self.mcp_attempts.remove(&turn);
        self.browser_launches.remove(&turn);
        self.spawning.remove(&turn);
        self.end_turn(
            turn,
            ManagedEnd::absent(TurnOutcome::NotLaunched { reason }),
        );
    }

    /// End an adapter-refused replay only when no earlier browser dispatch is unresolved.
    ///
    /// This is deliberately separate from [`Self::end_turn`]. There is no child to clean up and
    /// no launch claim to release: the adapter refused before [`Self::spawn`]. The only shared
    /// admission is the one-end guard, whose run-local half covers the window until this record
    /// is fsynced and whose folded half prevents a later replay.
    fn end_launch_refused(&mut self, turn: TurnId, error: &LaunchError) {
        if self.browser_turn_cancelled(turn) {
            return;
        }
        if self.retain_failed_browser_replay(turn) {
            return;
        }
        let Some(row) = self.world.turns.get(&turn) else {
            return;
        };
        if self.ending.contains(&turn) || row.has_ended() {
            return;
        }
        let (agent, goal) = (row.agent, row.goal);
        self.ending.insert(turn);
        tracing::warn!(
            %turn,
            %agent,
            launch_error = %error,
            "the adapter refused a replayed managed launch"
        );
        self.persist(
            Record::TurnEnded {
                final_reply: None,
                elapsed_ms: None,
                session_usage: None,
                agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                exit: ExitStatus::UNKNOWN,
                usage: None,
                model_usage: BTreeMap::new(),
                last_message: None,
                session: None,
                outcome: Some(TurnOutcome::LaunchRefused {
                    reason: error.to_string(),
                }),
            },
            self.after_turn_end(turn, agent, goal),
        );
    }

    /// The vendor named a session back to us, which is the only thing that makes one resumable.
    ///
    /// A `Resume(A)` accepts only `A`; a first launch accepts the vendor's admitted id even when it
    /// differs from the daemon's mint. Re-issued launches are first launches. A session already
    /// bound to another agent is refused before it can become a resumable argv value.
    fn on_session_learned(&mut self, turn: TurnId, session: &SessionId) {
        if self.browser_turn_cancelled(turn) {
            return;
        }
        let Some(turn_row) = self.world.turns.get(&turn).cloned() else {
            return;
        };
        if turn_row.has_ended() {
            return;
        }
        let agent = turn_row.agent;
        if self
            .world
            .agents
            .iter()
            .any(|(other, row)| *other != agent && row.session.as_ref() == Some(session))
        {
            tracing::warn!(
                %agent,
                %turn,
                "the vendor session is already bound to another agent"
            );
            return;
        }

        let reissued = self.reissued.contains_key(&turn);
        let confirmation_precedes_reissue_spawn = reissued && turn_row.is_issued_not_spawned();
        if !confirmation_precedes_reissue_spawn && turn_row.session_confirmed {
            return;
        }
        let announced = turn_row.announced.as_ref();
        let resumed = !reissued
            && announced.is_some()
            && self
                .world
                .agents
                .get(&agent)
                .and_then(|row| row.session.as_ref())
                == announced;
        if resumed && announced != Some(session) {
            tracing::warn!(
                %agent,
                %turn,
                category = "resume_session_mismatch",
                "the vendor did not confirm the session this turn resumed"
            );
            return;
        }
        if self.world.agents.get(&agent).is_none_or(|row| row.retired) {
            return;
        }
        if confirmation_precedes_reissue_spawn {
            self.reissue_confirmations
                .entry(turn)
                .or_insert_with(|| session.clone());
            return;
        }
        self.persist(
            Record::SessionLearned {
                agent,
                turn,
                session: session.clone(),
            },
            Then::Fold,
        );
    }

    fn persist_held_reissue_confirmation(&mut self, agent: AgentId, turn: TurnId) {
        let Some(session) = self.reissue_confirmations.remove(&turn) else {
            self.kill_standing_cause(agent);
            return;
        };
        let refused = self.world.agents.get(&agent).is_none_or(|row| row.retired)
            || self
                .world
                .agents
                .iter()
                .any(|(other, row)| *other != agent && row.session.as_ref() == Some(&session));
        if refused {
            self.kill_standing_cause(agent);
            return;
        }
        self.persist(
            Record::SessionLearned {
                agent,
                turn,
                session,
            },
            Then::KillStandingCause { agent },
        );
    }

    /// The reaper's answer about one recorded child.
    ///
    /// The runtime executes the probe; **every decision about what the answer means is here**, so
    /// a platform layer holds no classification and a second host cannot disagree with the first
    /// about when a turn is over.
    ///
    /// A late answer about a turn the machine is already ending, or the journal has closed, is
    /// inert. The world still calls an unacknowledged end `Spawned`, so both halves are required:
    /// neither liveness nor child ownership may be recreated after end teardown has begun.
    fn on_liveness(&mut self, turn: TurnId, liveness: Liveness) {
        let Some(row) = self.world.turns.get(&turn).cloned() else {
            return;
        };
        if self.ending.contains(&turn) || !row.is_open_child() {
            return;
        }
        match liveness {
            // Observed absence. `exit` is `UNKNOWN` and `usage` is `None` because nothing waited
            // on this process and no result line was ever read — a guessed exit code here would be
            // the `Inferred` end that no emitter writes.
            Liveness::Gone => {
                // `Vanished` is the reaper's own default, not the exit path's: no result line was
                // read here, so there is no adapter outcome to fall back to. The ledger still
                // wins over it — a turn this run killed and then found gone ends `Killed{by}`.
                self.end_turn(turn, ManagedEnd::absent(TurnOutcome::Vanished));
            }
            // **Nothing is written.** Not a record, not a status move: the turn stays open, the
            // agent stays `Running`, and the next beat asks again. Stuck-but-visible is what
            // is chosen over a false `Gone`. What is remembered is run-local, and it is
            // what makes a standing kill defer rather than signal a pid nothing can verify.
            Liveness::Unknown => {
                let state = match self.liveness.get(&turn).copied() {
                    Some(state @ LivenessState::Unknown { .. }) => state,
                    None | Some(LivenessState::Alive) => LivenessState::Unknown {
                        first_at: self.clock.now(),
                    },
                };
                self.liveness.insert(turn, state);
            }
            // Adopt. No record: `TurnSpawned` already says this child exists, and invariant 1's
            // projection is rebuilt from that. Repopulating `children` is the whole of it — after
            // a restart it is empty, which is why a standing cause against a previous run's child
            // had nothing to signal until this answer arrived.
            Liveness::Alive => {
                self.liveness.insert(turn, LivenessState::Alive);
                if let Some(key) = row.adoption_key() {
                    self.mcp_attempts.entry(turn).or_insert_with(|| McpAttempt {
                        grant: row.mcp_grant,
                        key: Some(key.clone()),
                        observations: Vec::new(),
                    });
                    self.children.entry(turn).or_insert(Child { key });
                }
                self.kill_standing_cause(row.agent);
            }
        }
    }

    /// **The child-backed constructor of `Persist{TurnEnded}` in this crate.** Its no-child
    /// sibling is [`Self::end_launch_refused`]. Answers nothing; refuses silently.
    ///
    /// Two child-backed paths end a managed turn — the adapter's [`Input::ChildExited`] and the
    /// reaper's `Gone` — and a turn must have exactly one end. The refusal lives here rather than
    /// in either handler so that a third child-backed path inherits it instead of forgetting it.
    /// The named tests in `tests/reaper.rs` pin both effects:
    /// `a_late_child_exited_does_not_append_a_second_turn_ended`
    /// catches the duplicate record, and `a_refused_end_keeps_the_deferral_it_already_journaled`
    /// catches cleanup reaching the once-per-cause guard. Liveness and child ownership cannot be
    /// recreated after the accepted end removes them; the two `late_*` liveness tests pin that
    /// separate input guard.
    ///
    /// **Why the run-local set is load-bearing.** `World::apply` moves a turn's status from the
    /// *acknowledged* entry, so a guard written as "refuse unless this is still an open child"
    /// reads a row that has not moved yet: between a reaper end being emitted and its fsync — the
    /// exact crash window the ledger exists to model — a `ChildExited` would sail through it and
    /// append a second `TurnEnded`. That second record would not be merely redundant. `kills` was
    /// consumed by the first end, so it would carry the adapter's outcome and supersede the
    /// correct row last-write-wins by `lsn`, turning a `Killed{Halt}` into a `Completed`.
    ///
    /// Which is why **the refusal is the first thing that happens**: a refused end must leave
    /// every run-local map exactly as it found it.
    ///
    /// **`outcome` is the caller's default, and the two callers' defaults differ on purpose.**
    /// The ledger's `Killed{by}` overrides it whenever this run signalled the process — one line,
    /// so a turn the daemon killed reads the same however its end was noticed — but what it falls
    /// back to when the ledger is silent is not one value. `on_child_exited` has the vendor's
    /// `result` line to fall back to and passes the adapter's reading; the reaper has read no
    /// result line at all and passes `Vanished`, which is the vocabulary for a child that went
    /// missing rather than one that reported how it finished. Copying either call site onto the
    /// other would put an invented outcome in the journal.
    ///
    /// Note also that a reaper-written `Killed` carries `exit: UNKNOWN`: nothing waited on the
    /// process, so no reader and no projection may take `Killed` to imply a signal number.
    fn end_turn(&mut self, turn: TurnId, end: ManagedEnd) {
        if self.world.browser_issue(turn).is_some_and(|issue| {
            matches!(
                issue.cancellation,
                Some(crate::world::BrowserIssueCancellation::Clean { .. })
            )
        }) {
            return;
        }
        let ManagedEnd {
            elapsed_ms,
            exit,
            usage,
            model_usage,
            outcome,
            last_message,
            final_reply,
        } = end;
        let Some(row) = self.world.turns.get(&turn).cloned() else {
            return;
        };
        if self.ending.contains(&turn) || row.has_ended() {
            return;
        }
        self.ending.insert(turn);
        self.children.remove(&turn);
        self.mcp_attempts.remove(&turn);
        self.liveness.remove(&turn);
        // `self.deferred` is deliberately **not** one of these either. Its entry is the
        // once-per-cause guard and remains until the end's acknowledged entry hands ownership to
        // the fold. A late liveness answer cannot use it: `ending` now refuses that input before
        // either liveness or child ownership can be recreated.
        //
        // `self.spawning` is deliberately **not** one of these, and re-adding it is the mutant.
        // Nothing in this crate's contract orders `Input::Spawned` before `Input::ChildExited` —
        // `Spawned` is "an observation after the fact; nothing waits on it" — so a turn whose
        // launch is still outstanding reaches this function, and dropping it from the set here
        // would spend invariant 2's run-local guard on a launch the host never answered. The two
        // answering inputs own their clears: acknowledged `TurnSpawned` hands a successful claim
        // to the persistent fold, while `Input::SpawnFailed` clears its confirmed-absence claim in
        // its own handler.

        // The adapter read the vendor's `result` line; the ledger knows whether we signalled the
        // process. Ours wins, which is what makes `TurnStatus::Killed` reachable.
        let outcome = self
            .kills
            .remove(&turn)
            .map_or(outcome, |by| TurnOutcome::Killed { by });
        // Until a re-issue's `TurnSpawned` folds, the row still describes the crashed attempt,
        // and only the run-local entries say what this attempt announced and what confirmed it.
        let reissue = self
            .reissued
            .get(&turn)
            .filter(|_| row.is_issued_not_spawned())
            .cloned();
        // A running total is taken off only a session the vendor confirmed for this turn. A
        // refused resume ran on another thread, and a re-issue announces a fresh id or none,
        // so neither owes anything to a recorded total.
        let recorded = match (&reissue, &row.session) {
            (None, Some(session)) if row.session_confirmed => self.world.session_total(session),
            _ => Usage::ZERO,
        };
        let session = reissue.unwrap_or_else(|| row.ended_session());
        let session_usage = match usage {
            Some(ScopedUsage::Session { total, .. }) => Some(total),
            Some(ScopedUsage::Turn(_)) | None => None,
        };
        let usage = usage.map(|reported| reported.charge(recorded));
        let final_reply = final_reply.filter(|_| self.world.browser_issue(turn).is_some());
        let last_message = if final_reply.is_some() {
            None
        } else {
            last_message
        };
        self.persist(
            Record::TurnEnded {
                final_reply,
                elapsed_ms,
                session_usage,
                agent: row.agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                exit,
                usage,
                model_usage,
                last_message,
                session,
                outcome: Some(outcome),
            },
            self.after_turn_end(turn, row.agent, row.goal),
        );
    }

    fn protection_pending(&self, agent: AgentId) -> bool {
        self.ledger
            .values()
            .any(|parked| match &parked.entry.record {
                Record::AgentHeld { agent: held, .. } => *held == agent,
                Record::BreakerBeat {
                    agent: held,
                    signals,
                    ..
                } => *held == agent && signals.contains(&BreakerSignal::RapidFailure),
                _ => false,
            })
    }

    fn launch_held(&self, agent: AgentId) -> bool {
        self.protection_pending(agent)
            || self
                .world
                .agents
                .get(&agent)
                .is_some_and(|row| row.automatic_hold.is_some())
    }

    fn hold_agent(
        &mut self,
        agent: AgentId,
        provenance: Provenance,
        cause: dudes_proto::AutomaticHold,
    ) {
        self.persist(
            Record::AgentHeld {
                agent,
                provenance,
                cause,
            },
            Then::Decide { agent },
        );
    }

    fn protect_failures(&mut self, agent: AgentId) -> bool {
        if self.protection_pending(agent) {
            return true;
        }
        let Some(row) = self.world.agents.get(&agent) else {
            return false;
        };
        if row.retired || row.automatic_hold.is_some() {
            return false;
        }
        if let Some(cause) = row.pending_launch_failure.clone() {
            self.hold_agent(agent, Provenance::Observed, cause);
            return true;
        }
        let Some((first, latest)) = row.fast_failures.filter(|(first, latest)| first != latest)
        else {
            return false;
        };
        if row.rapid_failure_beat {
            self.hold_agent(
                agent,
                Provenance::Inferred,
                dudes_proto::AutomaticHold::RepeatedFastFailure { first, latest },
            );
        } else {
            let rung_before = row.rung;
            let signals = vec![BreakerSignal::RapidFailure];
            let rung_after = dudes_policy::beat(rung_before, &signals, false, self.config.breaker);
            self.persist(
                Record::BreakerBeat {
                    agent,
                    rung_before,
                    rung_after,
                    signals,
                },
                Then::HoldAfterRapidFailureBeat {
                    agent,
                    first,
                    latest,
                },
            );
        }
        true
    }

    /// After a turn end: run the goal's predicate, or re-decide when the goal is gone.
    fn judge(&mut self, agent: AgentId, goal: GoalId) {
        if self.protect_failures(agent) {
            return;
        }
        let predicate = self
            .world
            .goals
            .get(&goal)
            .filter(|row| !row.cleared)
            .map(|row| (row.goal.predicate.clone(), row.agent));
        let cwd = self.world.agents.get(&agent).map(|row| row.cwd.clone());
        match (predicate, cwd) {
            (Some((predicate, judged_agent)), Some(cwd)) => {
                if matches!(predicate, Predicate::BoardState { .. }) {
                    self.goals_awaiting_board_result.insert(goal);
                }
                self.pending.push(Effect::RunPredicate {
                    goal,
                    agent: judged_agent,
                    cwd,
                    predicate,
                });
            }
            _ => self.decide(agent),
        }
    }

    fn on_predicate_done(&mut self, goal: GoalId, pass: bool, evidence: Evidence) {
        let Some(row) = self.world.goals.get(&goal) else {
            return;
        };
        let (agent, turn) = (row.agent, row.last_turn);
        // Filtered here as well as in the fold, so the journaled verdict and every projection of
        // it say what the loop decided on: an observed failure.
        let pass = pass && crate::predicate_evidence_eligible(&row.goal.predicate, &evidence);
        self.persist(
            Record::PredicateResult {
                goal,
                turn,
                // `Observed` by construction: the daemon ran it. Only an `Observed` pass is
                // `Done` (invariant 3), and this is the one emitter of a predicate result.
                provenance: Provenance::Observed,
                pass,
                evidence,
            },
            Then::Decide { agent },
        );
    }

    // ---- the breaker ------------------------------------------------------------------------

    /// Collect one signal. Nothing moves here; the ladder moves on the beat.
    ///
    /// The collection is the point. A rung that moved per event would climb four rungs on four
    /// signals in one second, and "one rung per beat" would hold only inside
    /// `dudes_policy::beat()`, whose caller could still ring it like a bell.
    ///
    /// Nothing is filtered here, deliberately: [`Self::on_tick`] decides who has a breaker, and a
    /// second copy of that filter in front of the map would be a rule no test could tell from its
    /// absence.
    fn on_signal(&mut self, agent: AgentId, signal: RuntimeSignal) {
        self.signals
            .entry(agent)
            .or_default()
            .insert(signal.signal());
    }

    /// **The order is load-bearing, not presentation.**
    /// [`Self::observe_isolation`] ends in [`Self::beat`], and `beat` *drains* the agent's signal
    /// set, so a billing insert placed after it would land in a freshly emptied set and wait for
    /// the next tick — two `BreakerBeat` records for one init. Inserting first, the billing arm
    /// rides whatever beat the isolation arm makes when there is one, and an init carrying both
    /// observations journals a single beat carrying both variants, sorted by declaration order.
    fn on_session_observed(
        &mut self,
        turn: TurnId,
        key: AdoptionKey,
        mcp_grant: Option<Lsn>,
        observation: SessionObservation,
    ) {
        if self.browser_turn_cancelled(turn) {
            return;
        }
        let Some(row) = self.world.turns.get(&turn) else {
            return;
        };
        if row.has_ended() || self.ending.contains(&turn) {
            return;
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = self.mcp_attempts.entry(turn) {
            let Some(recorded_key) = row
                .adoption_key()
                .filter(|recorded| same_child_key(recorded, &key))
            else {
                return;
            };
            if row.mcp_grant != mcp_grant {
                return;
            }
            entry.insert(McpAttempt {
                grant: mcp_grant,
                key: Some(recorded_key),
                observations: Vec::new(),
            });
        }
        let attempt = self
            .mcp_attempts
            .get_mut(&turn)
            .expect("the physical attempt was established");
        if attempt.grant != mcp_grant {
            return;
        }
        match &attempt.key {
            None => {
                attempt.observations.push((key, observation));
                return;
            }
            Some(expected) if !same_child_key(expected, &key) => return,
            Some(_) => {}
        }
        let Some(agent) = self.world.turns.get(&turn).map(|row| row.agent) else {
            return;
        };
        if mcp_grant.is_some_and(|lsn| {
            self.world
                .mcp_grant(lsn)
                .is_none_or(|grant| grant.agent != agent || grant.turn != turn)
        }) {
            return;
        }
        if let Some(session) = &observation.session {
            self.on_session_learned(turn, session);
        }
        let Some(provider) = self.world.agents.get(&agent).map(|row| row.provider) else {
            return;
        };
        self.observe_billing(agent, turn, observation.credential.as_ref());
        self.observe_isolation(agent, turn, provider, mcp_grant, &observation);
    }

    /// Latch the vendor's reported credential source, and raise a rung while nothing caps it.
    ///
    /// The signal is raised when **both** hold: the vendor reported an API key, and
    /// no ceiling is set in any scope.
    ///
    /// **This arm raises into [`Self::signals`] and calls no [`Self::beat`].** An out-of-band beat
    /// is an extra beat, and beats are where descents happen too, so a path that beat on its own
    /// input could hold a ladder down by raising. Insertion into the set is monotone until the
    /// tick drains it; The `RuntimeSignal` doc authorises the direct path.
    ///
    /// The observed value is logged at `debug` on **every** managed init, outside the conjunction:
    /// gating the only emission behind the raise would leave what a ceiling-ed agent reports
    /// unmeasurable in a deployment. A `debug` line is no journal, no projection and no persistent
    /// state, so invariant 1 is not reached. The `warn` is the stream-line observation's alone —
    /// the tick's re-raise is silent, and what records a held condition is the `BreakerBeat`.
    fn observe_billing(&mut self, agent: AgentId, turn: TurnId, credential: Option<&Credential>) {
        if matches!(credential, Some(Credential::ApiKey(_))) {
            self.last_session_on_api_key.insert(agent);
        } else {
            self.last_session_on_api_key.remove(&agent);
        }
        tracing::debug!(
            ?agent,
            ?turn,
            source = ?credential,
            "observed the vendor's reported credential source on a managed init"
        );
        if self.uncapped_billing_source(agent) {
            tracing::warn!(
                ?agent,
                ?turn,
                source = ?credential,
                "a managed turn reported a billing credential source and no ceiling is set in any scope"
            );
            self.raise_uncapped_billing(agent);
        }
    }

    /// Insert the signal for `agent`, with no latch write and no log: the tick's re-raise.
    fn raise_uncapped_billing(&mut self, agent: AgentId) {
        self.signals
            .entry(agent)
            .or_default()
            .insert(BreakerSignal::UncappedBillingSource);
    }

    /// Both halves of the uncapped-billing predicate, read from folded state alone.
    ///
    /// No I/O, no clock and no journal read: invariant 5 is untouched, because what reaches
    /// `dudes_policy` is a `BreakerSignal` in the slice `beat` already takes, never this question.
    fn uncapped_billing_source(&self, agent: AgentId) -> bool {
        self.last_session_on_api_key.contains(&agent) && self.no_ceiling_anywhere(agent)
    }

    /// Whether no ceiling is set for `agent` in any scope or unit reachable from folded state.
    ///
    /// **`Option::is_some`, never `> 0`.** A ceiling has three states per
    /// unit — absent, zero and set — and **zero is set**: it means *spend nothing*, not *spend
    /// anything*. A predicate written `> 0` would raise on the one configuration that caps
    /// hardest.
    ///
    /// The twin of the daemon's admission query, and deliberately
    /// not shared with it: that one also reads the call's own `HireParams`/`GoalSetParams` budget,
    /// which this fold never sees. Two inputs, two times, one question.
    ///
    /// An agent with no goal has no goal-scoped ceiling, which is what `is_none_or` says here.
    fn no_ceiling_anywhere(&self, agent: AgentId) -> bool {
        if self.config.floor_usd_per_day.is_some() {
            return false;
        }
        let Some(row) = self.world.agents.get(&agent) else {
            return false;
        };
        if row.budget.usd_per_day.is_some() || row.budget.tokens_per_day.is_some() {
            return false;
        }
        row.goal
            .and_then(|goal| self.world.goals.get(&goal))
            .is_none_or(|goal| goal.goal.budget.usd.is_none() && goal.goal.budget.tokens.is_none())
    }

    /// Treat what a managed turn reported about its tool servers and plugins as runtime evidence
    /// for the adapter's declared isolation, against the servers the agent was opted in to.
    ///
    /// A server the agent was not opted in to, or any plugin, raises the leak. A declared server
    /// the vendor's whole list leaves out, or lists as not connected, narrows the turn rather than
    /// widening it, so it is logged and raises nothing. Read from folded state alone, with no I/O.
    ///
    /// The adapter registry is the posture's one owner, and `Isolation::None` deliberately accepts
    /// inherited servers and plugins. Unaffected by the uncapped-billing signal: its own [`Self::beat`] call stays
    /// where it was, and removing it is a separate question, not this one's.
    fn observe_isolation(
        &mut self,
        agent: AgentId,
        turn: TurnId,
        provider: Provider,
        mcp_grant: Option<Lsn>,
        observation: &SessionObservation,
    ) {
        let isolated = self.adapters.get(provider).is_some_and(|adapter| {
            matches!(
                adapter.isolation(),
                Isolation::Flags(_) | Isolation::ConfigDir
            )
        });
        if !isolated {
            return;
        }
        let declared: &[dudes_types::McpServerName] = match mcp_grant {
            Some(lsn) => match self.world.mcp_grant(lsn) {
                Some(grant) if grant.agent == agent && grant.turn == turn => {
                    grant.mcp_servers.as_slice()
                }
                _ => return,
            },
            None => match self.world.agents.get(&agent) {
                Some(row) => row.mcp.hire_servers.as_slice(),
                None => return,
            },
        };
        let is_declared = |server: &str| declared.iter().any(|name| name.as_str() == server);
        if let Some(connected) = &observation.connected {
            let down: Vec<&str> = declared
                .iter()
                .map(dudes_types::McpServerName::as_str)
                .filter(|name| !connected.iter().any(|up| up == name))
                .collect();
            if !down.is_empty() {
                tracing::warn!(
                    ?agent,
                    ?turn,
                    servers = ?down,
                    "a declared MCP server is missing from a managed turn or did not connect"
                );
            }
        }
        let undeclared: Vec<&str> = observation
            .servers
            .iter()
            .map(String::as_str)
            .filter(|server| !is_declared(server))
            .collect();
        if undeclared.is_empty() && observation.plugins.is_empty() {
            return;
        }

        let plugins: Vec<&str> = observation
            .plugins
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        tracing::warn!(
            ?agent,
            ?turn,
            servers = ?undeclared,
            ?plugins,
            plugin_count = observation.plugins.len(),
            "a managed turn loaded servers or plugins the agent was not given"
        );
        self.signals
            .entry(agent)
            .or_default()
            .insert(BreakerSignal::IsolationLeak);
        self.beat(agent);
    }

    /// One beat per non-retired agent, then the reap, then the re-examination.
    ///
    /// A retired agent has no breaker: its ladder stops where it stood, and gate 5 never reads it
    /// again because `World::loop_input` decides nothing for a retired agent at all. `AgentRetired`
    /// is itself a standing kill cause, so a turn it left running is ended by that.
    fn on_tick(&mut self) {
        let agents: Vec<AgentId> = self
            .world
            .agents
            .iter()
            .filter(|(_, row)| !row.retired)
            .map(|(agent, _)| *agent)
            .collect();
        for agent in agents {
            // the level is re-read here, before the beat that would otherwise
            // drain the last raise away. `observe_billing` latched it; this asks the same
            // question of latch × ceiling again and re-raises while it holds, so an uncleared
            // condition climbs a rung per beat instead of oscillating against the descent. An
            // operator setting a ceiling in any scope, or a later session reporting no API key, makes
            // the predicate false on the next beat and the ladder walks back down on its own.
            if self.uncapped_billing_source(agent) {
                self.raise_uncapped_billing(agent);
            }
            self.beat(agent);
        }
        // Each beat drained its own agent. What can still be here was collected for an agent with
        // no ladder — retired, or never hired — and is dropped, so the map never holds more than
        // one beat's worth.
        self.signals.clear();
        self.reap();
        self.re_examine_exhausted();
    }

    /// The third part of the beat: re-decide an `Exhausted` agent whose loop input
    /// has changed since the decision that exhausted it.
    ///
    /// Without this the transition `agent_fsm` has always asserted — *"tomorrow's budget
    /// re-decides; `Exhausted` is not a trap"* — has no producer: the rung walks back down to
    /// `Healthy` across quiet beats, the UTC day rolls over, and the agent stays `Exhausted` for
    /// the life of the daemon because nothing asks again.
    ///
    /// **It adds no gate and no policy clause.** The guard asks one question — *did the input
    /// change?* — and [`Self::decide`] answers the other — *and does that change the verdict?* So
    /// the rule is cause-agnostic by construction rather than by a list: a breaker that recovered,
    /// a day that rolled over and a goal ceiling that was raised all differ in the digest, and a
    /// hop cap does not, because `turns_on_goal` only grows. Nothing here names an
    /// `ExhaustedReason`, so nothing here needs editing when one is added.
    ///
    /// **Invariant 5 holds, twice over.** Nothing new enters `dudes-policy` — this calls the same
    /// `decide()` every other path calls, over an input the same [`Self::loop_input_for`] built.
    /// And **nothing that decides whether to re-examine reads a clock**: the candidate filter and
    /// the digest comparison run entirely off the fold, and `Input::Tick` carries no time value.
    /// The decision that follows a changed digest then stamps its record like every other decision
    /// in the crate. The tick's earlier breaker phase may read the clock to resolve the
    /// run-local compaction deadline, but that value does not enter this candidate filter or the
    /// digest. The day rollover is not a clock read here either: `SpendView.agent_today` arrives
    /// *already rolled over* on
    /// [`Input::Spend`](crate::Input::Spend), which the runtime pushes before every input including
    /// the tick — which is also why this fix is reachable in production and not only in a harness.
    ///
    /// **One beat of latency, deliberately**, and the rule is general: **any digest
    /// input journaled inside this same `handle(Input::Tick)` is seen one beat late.** This reads
    /// the fold, and a record parked `Then::Fold` is journaled but not yet acknowledged, so
    /// `World::apply` has not run for it when the sweep looks. Today that is the beat's
    /// `BreakerBeat` — so a recovered rung resumes the loop on the *next* beat, at most one beat
    /// interval later — and [`Self::reap`], which runs immediately above and can
    /// park a `KillDeferred` the same way. Stated as the general form on purpose: the next thing
    /// added to the tick inherits this latency rather than introducing a new one, and reading it as
    /// new would send someone looking for a second mechanism that is not there.
    ///
    /// One mechanism instead of two, and one beat of extra patience for an agent that was just
    /// misbehaving is not a cost anyone can measure. An input the tick does *not* journal — the
    /// spend, pushed as [`Input::Spend`](crate::Input::Spend) and folded before the tick arrives,
    /// and now gate 7's work on [`Input::Work`](crate::Input::Work) beside it — carries no such
    /// delay, which is why the day-roll arm re-decides on the first beat and the breaker arm on
    /// the second.
    ///
    /// The four conjuncts of the re-decide guard, written out here —
    /// **and three of them cannot be observed to do anything**, so a reader should not read them
    /// as protections with tests behind them:
    ///
    /// - `Exhausted` — the only one with a mutant, and it is pinned by
    ///   `tests/exhausted_sweep.rs::an_agent_that_is_not_exhausted_is_not_re_examined`. Widen it and
    ///   an `Idle` agent between turns is re-decided while its predicate is still running, so the
    ///   loop issues a second turn for a goal it has not finished judging.
    /// - `!row.retired` — [`Self::loop_input_for`] already returns `None` for a retired agent, so
    ///   deleting this changes nothing observable.
    /// - `!row.attached` — `agent_fsm::next` gives `Attached` priority from every state, so an
    ///   agent cannot be `Exhausted` and attached at once; the combination is unreachable.
    /// - `open_turn.is_none()` — [`Self::decide`] early-returns on an open turn, and issuing one
    ///   moves the status to `Running`, so this can never be the thing that stops a decision.
    fn re_examine_exhausted(&mut self) {
        let candidates: Vec<AgentId> = self
            .world
            .agents
            .iter()
            .filter(|(agent, row)| {
                self.status_of(**agent) == AgentStatus::Exhausted
                    && !row.retired
                    && !row.attached
                    && row.open_turn.is_none()
            })
            .map(|(agent, _)| *agent)
            .collect();
        for agent in candidates {
            if self
                .world
                .agents
                .get(&agent)
                .is_some_and(|row| row.goal.is_none())
            {
                if let Some(computed) = self
                    .conversation_input_for(agent)
                    .as_ref()
                    .and_then(conversations::conversation_digest)
                    && self.world.conversation_input_digest(agent) != Some(computed.as_str())
                {
                    self.decide(agent);
                }
                continue;
            }
            if self.world.completed_goal(agent).is_some() {
                let Some(input) = self.completed_note_input_for(agent) else {
                    self.request_mail_wake(agent);
                    continue;
                };
                let computed = match crate::host::taught_note_input_digest(&input) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::error!(%agent,%error,"taught_note_digest_refused");
                        continue;
                    }
                };
                let changed = self.world.note_admission(agent).is_none_or(|row| {
                    row.contract == dudes_proto::NoteContract::Raw || row.input_digest != computed
                });
                if changed {
                    self.decide(agent);
                }
                continue;
            }
            let computed = self.loop_input_for(agent).and_then(|input| {
                if self.closing_active {
                    let session = self
                        .world
                        .closing
                        .as_ref()
                        .filter(|session| !session.is_complete())?;
                    let participant = session.participants.get(&agent)?;
                    let goal = self.world.agents.get(&agent)?.goal?;
                    if participant.saving_turn.is_some()
                        || participant.acknowledged.is_some()
                        || participant.goal != Some(goal)
                    {
                        return None;
                    }
                    crate::host::closing_input_digest(&dudes_policy::closing_input(
                        session.announcement,
                        agent,
                        goal,
                        input,
                    ))
                } else {
                    input_digest(&input)
                }
            });
            let stored = self
                .world
                .agents
                .get(&agent)
                .and_then(|row| row.last_input_digest.as_deref());
            if digest_changed(stored, computed.as_deref()) {
                self.decide(agent);
            }
        }
    }

    /// The reaper's half of the beat: ask again about every open child the reaper owns or has yet
    /// to resolve, and retry a kill the last answer made us defer.
    ///
    /// **A live child of this run is deliberately not probed**, and that is a rule rather than an
    /// optimisation. The runtime is its parent and will deliver [`Input::ChildExited`]; a `Gone`
    /// racing an undelivered exit would be a second end for one turn, which is forbidden.
    /// So the set is: a turn with no entry in `children` (a previous run's child, not yet
    /// re-adopted) or one with an entry in `liveness` (the reaper answered about it, so the reaper
    /// owns it). That is "unresolved or adopted" written in state the machine already holds,
    /// which is why `Child` needs no `adopted` flag.
    ///
    /// The retry is here and not in `on_liveness`'s `Unknown` arm, deliberately: that arm writes
    /// nothing, so a deferral driven from it would be written once by construction and the
    /// once-per-cause guard would be untestable — it would pass with the guard deleted. Driven
    /// from the beat, deleting the guard turns one record into one per beat, which is the ~2,880
    /// a day the once-per-cause guard exists to prevent.
    fn reap(&mut self) {
        let probes: Vec<(TurnId, AgentId, AdoptionKey)> = self
            .world
            .open_children()
            .filter(|(turn, _)| {
                !self.children.contains_key(turn) || self.liveness.contains_key(turn)
            })
            .filter_map(|(turn, row)| row.adoption_key().map(|key| (turn, row.agent, key)))
            .collect();
        for (turn, agent, key) in probes {
            self.pending.push(Effect::Probe { turn, key });
            if self
                .liveness
                .get(&turn)
                .copied()
                .is_some_and(LivenessState::is_unknown)
            {
                self.kill_standing_cause(agent);
            }
        }
    }

    /// One agent's beat: move the ladder over the signals collected since its last one, and
    /// journal the move.
    ///
    /// **Journaled when the rung moves or a signal was seen**, and not otherwise: a healthy agent
    /// already at `Healthy` writes nothing, so an idle floor does not append two records a minute
    /// per agent for the rest of its life. A quiet beat that *lowers* the rung is a move and is
    /// written — recovery is a fact about the agent, and invariant 1 says a fact that is not a
    /// record did not happen.
    fn beat(&mut self, agent: AgentId) {
        if self
            .world
            .mail_wake_state(agent)
            .is_some_and(|state| state.fifth_crossing > state.fifth_consumed)
        {
            self.signals
                .entry(agent)
                .or_default()
                .insert(BreakerSignal::UnackedMail);
        }
        let signals: Vec<BreakerSignal> = self
            .signals
            .remove(&agent)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let Some(row) = self.world.agents.get(&agent) else {
            return;
        };
        let rung_before = row.rung;
        let compacting = self.compaction.get_mut(&agent).is_some_and(|compaction| {
            compaction.resolve(
                self.clock.now(),
                &signals,
                self.compaction_max_suppressed_beats,
            )
        });
        let rung_after = dudes_policy::beat(rung_before, &signals, compacting, self.config.breaker);
        if rung_after == rung_before && signals.is_empty() {
            return;
        }
        let stop = rung_after == BreakerRung::Stop;
        let isolation_leak = signals.contains(&BreakerSignal::IsolationLeak);
        self.persist(
            Record::BreakerBeat {
                agent,
                rung_before,
                rung_after,
                signals,
            },
            // `Stop` is a standing kill cause. Parked rather than acted on here: the rung the
            // kill rests on is read back out of the world, and the world only holds it once the
            // beat is fsynced — every effect journaled before it is acted on.
            if isolation_leak {
                Then::IsolationLeak { agent, stop }
            } else if stop {
                Then::KillStandingCause { agent }
            } else {
                Then::Fold
            },
        );
    }

    // ---- commands ---------------------------------------------------------------------------

    #[allow(clippy::needless_pass_by_value)]
    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive command table stays in one place; additive record provenance crossed the lint threshold"
    )]
    fn on_command(&mut self, command: Command) {
        match command {
            Command::ConversationMessageQueued {
                agent,
                text,
                browser,
                context_lsn,
                context_reset_turn,
            } => {
                if self.browser_delivery_open {
                    let record = match context_reset_turn {
                        Some(context_reset_turn) => Record::ConversationContextResetQueued {
                            agent,
                            text,
                            browser,
                            context_lsn,
                            context_reset_turn,
                            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
                        },
                        None => Record::ConversationMessageQueued {
                            agent,
                            text,
                            browser,
                            context_lsn,
                            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
                        },
                    };
                    self.persist(record, Then::Decide { agent });
                }
            }
            Command::BrowserNoteQueued {
                agent,
                text,
                browser,
                destination,
            } => {
                if self.browser_delivery_open {
                    self.persist(
                        Record::BrowserNoteQueued {
                            agent,
                            text,
                            browser,
                            destination,
                        },
                        Then::Decide { agent },
                    );
                }
            }
            Command::BrowserNoteCancel {
                agent,
                note_lsn,
                browser,
            } => self.cancel_browser_note(agent, note_lsn, browser),
            Command::BrowserOnboardHire(input) => {
                let input = *input;
                let id = AgentId::from_ulid(self.rng.ulid());
                let intended_goal = Goal {
                    completion_check: input.completion_check,
                    id: GoalId::from_ulid(self.rng.ulid()),
                    text: input.goal_text,
                    predicate: input.predicate,
                    budget: input.goal_budget,
                    hop_cap: input.hop_cap,
                    baseline: input.baseline,
                };
                self.persist(
                    Record::AgentHired {
                        character: input.character,
                        accent: input.accent,
                        id,
                        name: input.name,
                        role: input.role,
                        department: input.department,
                        provider: input.provider,
                        cwd: input.cwd,
                        workspace: Some(dudes_proto::record::RecordedWorkspace {
                            id: input.profile_id.clone(),
                            label: input.workspace_label,
                        }),
                        budget: input.budget,
                        sandbox: None,
                        isolation: input.isolation,
                        max_turns: None,
                        max_budget: None,
                        trusted: false,
                        worktree: None,
                        mcp_servers: Vec::new(),
                        browser_onboarding: Some(Box::new(BrowserOnboardingHire {
                            attempt_id: input.attempt_id,
                            request_digest: input.request_digest,
                            profile_id: input.profile_id,
                            session_digest: input.session_digest,
                            intended_goal,
                        })),
                    },
                    Then::Fold,
                );
            }
            Command::BrowserOnboardGoal(input) => {
                let input = *input;
                self.persist(
                    Record::GoalSet {
                        agent: input.agent,
                        goal: input.goal,
                        browser_onboarding: Some(BrowserOnboardingGoal {
                            attempt_id: input.attempt_id,
                            session_digest: input.session_digest,
                        }),
                        browser_goal: None,
                        goal_replacement: None,
                    },
                    Then::Decide { agent: input.agent },
                );
            }
            Command::Hire {
                character,
                accent,
                name,
                role,
                department,
                provider,
                cwd,
                workspace,
                budget,
                sandbox,
                isolation,
                max_turns,
                max_budget,
                trusted,
                worktree,
                mcp_servers,
            } => {
                let id = AgentId::from_ulid(self.rng.ulid());
                self.persist(
                    Record::AgentHired {
                        character,
                        accent,
                        id,
                        name,
                        role,
                        department,
                        provider,
                        cwd,
                        workspace,
                        budget,
                        sandbox,
                        isolation,
                        max_turns,
                        max_budget,
                        trusted,
                        worktree,
                        mcp_servers,
                        browser_onboarding: None,
                    },
                    Then::Fold,
                );
            }
            Command::LeaseWorktree { origin, base, from } => {
                // A ULID leads with its millisecond timestamp, and the branch name takes the first
                // 12 hexadecimal characters of the identity, so two leases made in one millisecond
                // would share a branch. The random end leads instead.
                let mut bytes = self.rng.ulid().to_bytes();
                bytes.reverse();
                let worktree = WorktreeId::from_bytes(bytes);
                self.persist(
                    Record::WorktreeLeased {
                        worktree,
                        origin,
                        path: self
                            .config
                            .data_dir
                            .join("worktrees")
                            .join(worktree.to_string()),
                        branch: format!("dudes/{}", worktree.short()),
                        base,
                        from,
                    },
                    Then::Fold,
                );
            }
            Command::RemoveWorktree { worktree, how } => {
                use crate::world::WorktreeState::{Pending, Released};
                use dudes_proto::WorktreeRemoval::{NeverAssigned, Operator};
                let allowed = matches!(
                    (how, self.world.worktree_state(worktree)),
                    (NeverAssigned, Some(Pending)) | (Operator, Some(Released))
                );
                if allowed {
                    self.persist(Record::WorktreeRemoved { worktree, how }, Then::Fold);
                }
            }
            Command::Rename {
                agent,
                name,
                browser_rename,
            } => self.persist(
                Record::AgentRenamed {
                    id: agent,
                    name,
                    browser_rename,
                },
                Then::Fold,
            ),
            Command::Look {
                agent,
                role,
                character,
                accent,
                browser_look,
            } => self.persist(
                Record::AgentLookChanged {
                    id: agent,
                    role,
                    character,
                    accent,
                    browser_look,
                },
                Then::Fold,
            ),
            Command::ReplaceMcp { agent, mcp_servers } => {
                let Some(row) = self.world.agents.get(&agent) else {
                    return;
                };
                if row.retired
                    || row.mcp.browser_onboarded
                    || row.mcp.desired == mcp_servers
                    || mcp_servers
                        .windows(2)
                        .any(|pair| matches!(pair, [first, second] if first >= second))
                    || (row.mcp.first_edit.is_none() && self.has_ungranted_browser_launch(agent))
                {
                    return;
                }
                self.persist(
                    Record::AgentMcpServersChanged { agent, mcp_servers },
                    Then::Fold,
                );
            }
            Command::Retire {
                agent,
                reason,
                browser_retire,
            } => self.persist(
                Record::AgentRetired {
                    id: agent,
                    reason,
                    browser_retire,
                },
                Then::Retired { agent },
            ),
            Command::GoalSet {
                agent,
                text,
                predicate,
                budget,
                hop_cap,
                browser_goal,
                goal_replacement,
                completion_check,
                baseline,
            } => {
                let goal = Goal {
                    completion_check,
                    id: GoalId::from_ulid(self.rng.ulid()),
                    text,
                    predicate,
                    budget,
                    hop_cap,
                    baseline,
                };
                let then = if goal_replacement.is_some() {
                    Then::Decide { agent }
                } else {
                    Then::ClearedCause { agent }
                };
                self.persist(
                    Record::GoalSet {
                        agent,
                        goal,
                        browser_onboarding: None,
                        browser_goal,
                        goal_replacement,
                    },
                    then,
                );
            }
            Command::GoalClear { agent, goal } => {
                self.persist(Record::GoalCleared { agent, goal }, Then::Fold);
            }
            // `loop.halt` stops the agent and ends a running turn; `loop.pause` only stops the
            // issuing — "hooks and mail keep working" — so it kills nothing.
            Command::Halt { agent, text } => self.persist(
                Record::Halt { agent, text },
                Then::KillStandingCause { agent },
            ),
            // A browser pause or resume is the same record with its attribution beside it, and
            // takes the same continuation as the local one.
            Command::Pause { agent, browser } => {
                self.persist(Record::Pause { agent, browser }, Then::Fold);
            }
            // Lifts a halt, a pause and an automatic hold, so it re-runs crash rule (2), as
            // `GoalSet` does for an automatic hold. A turn issued while the agent was halted was
            // filtered out of every re-issue since, so without this it would stay `Issued` until
            // the daemon restarted — a halt cleared hours ago still holding a turn.
            Command::Resume { agent, browser } => {
                self.persist(
                    Record::Resume { agent, browser },
                    Then::ClearedCause { agent },
                );
            }
            Command::Steer {
                agent,
                text,
                provenance,
            } => {
                // The cap is enforced here and nowhere else (`world::MAX_PENDING_STEERS`): a drop
                // is a record, and only the machine can append one. Both records go in this
                // order, so the fold that replays them reaches the queue the live run reached.
                for steer_lsn in self.steer_evictions(agent) {
                    self.steer_evicting
                        .entry(agent)
                        .or_default()
                        .insert(steer_lsn);
                    self.persist(Record::SteerCleared { agent, steer_lsn }, Then::Fold);
                }
                *self.steer_appending.entry(agent).or_default() += 1;
                self.persist(
                    Record::Steer {
                        agent,
                        text,
                        provenance,
                    },
                    Then::Fold,
                );
            }
            // The one producer of `Record::HumanQuestion`, and therefore the one path to gate 3.
            // `Then::Question` notifies every subscriber and re-decides, which is
            // what puts this agent — and only this agent — into `Waiting`.
            Command::Ask { agent, text } => {
                let origin = self
                    .world
                    .agents
                    .get(&agent)
                    .and_then(|row| row.open_turn)
                    .and_then(|turn| self.world.conversation_question_origin(turn));
                let record = origin.map_or_else(
                    || Record::HumanQuestion {
                        agent,
                        text: text.clone(),
                    },
                    |origin| Record::ConversationQuestion {
                        agent,
                        origin,
                        text: text.clone(),
                    },
                );
                self.persist(record, Then::Question { agent, text });
            }
            Command::Answer {
                agent,
                text,
                provenance,
            } => {
                if self.queue_conversation_answer(
                    agent,
                    &text,
                    dudes_proto::ConversationAnswerAuthority::Operator { provenance },
                ) {
                    return;
                }
                self.persist(
                    Record::HumanAnswer {
                        agent,
                        text,
                        provenance,
                        browser_answer: None,
                    },
                    Then::AnswerSaved { agent },
                );
            }
            Command::ConditionedEffectSuppressed {
                method,
                target,
                provenance,
                reason,
                payload,
            } => self.persist(
                Record::ConditionedEffectSuppressed {
                    method,
                    target,
                    provenance,
                    reason,
                    payload,
                },
                Then::Fold,
            ),
            Command::Attach {
                agent,
                by,
                provenance,
                cols,
                rows,
            } => self.attach(agent, by, provenance, cols, rows),
            Command::Detach {
                agent,
                by,
                provenance,
            } => {
                if !self
                    .world
                    .agents
                    .get(&agent)
                    .is_some_and(|row| row.attached)
                {
                    return;
                }
                self.persist(
                    Record::Detached {
                        agent,
                        by,
                        provenance,
                    },
                    Then::Decide { agent },
                );
            }
            // The Floor's attributed answer: the continuation of `Command::Answer` above, reached
            // through its own typed command so the conditioned sink can neither build nor reach it.
            // The writer has already checked the question it names is the one open.
            Command::BrowserAnswer(input) => {
                let BrowserAnswerCommand {
                    agent,
                    text,
                    attribution,
                } = *input;
                if self.queue_conversation_answer(
                    agent,
                    &text,
                    dudes_proto::ConversationAnswerAuthority::Browser {
                        browser: attribution.clone(),
                    },
                ) {
                    return;
                }
                self.persist(
                    Record::HumanAnswer {
                        agent,
                        text,
                        provenance: None,
                        browser_answer: Some(attribution),
                    },
                    Then::AnswerSaved { agent },
                );
            }
            Command::Subscribe { client } => {
                self.clients.insert(client);
            }
            Command::Unsubscribe { client } => {
                self.clients.remove(&client);
            }
        }
    }

    /// Kill the agent's child, when a cause stands and there is a child to signal.
    ///
    /// The cause is read from the world rather than passed in, so one function answers "is there
    /// a standing reason to end this turn right now" for a `Halt`, an `AgentRetired`, a breaker
    /// at `Stop`, and a child that appeared after any of them. One function is also what carries
    /// the deferral for the causes that can wait: a `Stop` defers exactly as a `Halt` does
    /// — a `KillDeferred` record, retried on every `Tick` while the cause stands — because there
    /// is no second place for it to be forgotten.
    ///
    /// `Retire` is not one of those, so this is two of the three, not all three. `agent.retire` *cannot* wait:
    /// on an agent whose open turn has `Unknown` liveness the
    /// call is refused with `-32004 Refused{gate: "liveness_unknown"}` in the RPC layer, so
    /// `Command::Retire` never reaches the machine and no deferral is ever owed here for it. The
    /// branch below is on the *liveness*, not on the cause, which is why there is no retire arm
    /// to read: the situation a deferred retire would describe cannot arrive.
    ///
    /// Only `Stop` kills. A rung at `Constrain` refuses the *next* turn at gate 5 and leaves the
    /// running one alone — that is the difference between the two top rungs.
    fn kill_standing_cause(&mut self, agent: AgentId) {
        let Some(row) = self.world.agents.get(&agent) else {
            return;
        };
        // Ordered so the reason names the cause an operator would recognise first: a retired or
        // halted agent whose breaker also reached `Stop` was ended by the operator's command.
        let by = if row.retired {
            KillReason::Retire
        } else if row.halted {
            KillReason::Halt
        } else if row.rung == BreakerRung::Stop {
            KillReason::BreakerStop
        } else {
            return;
        };
        let Some(turn) = row.open_turn else {
            return;
        };
        // a kill whose start value cannot be verified could hit a stranger, so the
        // cause is journaled as deferred instead — **once per (turn, standing cause)**. The retry
        // is `reap()`'s, on every beat, and this is the only thing between it and ~2,880 identical
        // records a day.
        //
        // Every other state signals. No entry: this run spawned the child and holds it unreaped,
        // so it is `Alive` by construction — the live path, unchanged. `Alive`: the reaper said
        // so. The stranger hazard on the adoption path is `ProcessHost::kill`'s re-validation,
        // not this machine's. `Gone` is not among them because no site writes it:
        // `on_liveness` inserts in two arms only — `Unknown` and `Alive` — and its `Gone` arm
        // ends the turn instead of remembering it. (`end_turn`'s teardown is not the reason and
        // could not be: it sits below a refusal that returns first, which the `a_refused_end_*`
        // tests pin.) An arm naming `Gone` here would be dead: with an `unreachable!()`
        // body, no test could reach it.
        if self
            .liveness
            .get(&turn)
            .copied()
            .is_some_and(LivenessState::is_unknown)
        {
            if self.deferred.get(&turn) == Some(&by) {
                return;
            }
            self.deferred.insert(turn, by);
            self.persist(
                Record::KillDeferred {
                    agent,
                    turn,
                    reason: KillDeferredReason::LivenessUnknown,
                },
                Then::Fold,
            );
            return;
        }
        let Some(child) = self.children.get(&turn) else {
            // No child to signal, for one of three reasons: the spawn has not happened yet — in
            // which case the `TurnSpawned` parks another `KillStandingCause` — or the turn already
            // ended, or the child belongs to a previous daemon run and has not been re-adopted.
            // The reaper's `Alive` answer is what repopulates `children` in that third case, and
            // it re-enters this function when it does.
            return;
        };
        self.kills.insert(turn, by);
        self.pending.push(Effect::Kill {
            agent,
            turn,
            key: child.key.clone(),
        });
    }

    // ---- hooks and notifications ------------------------------------------------------------

    fn activity_source_live(&self, agent: AgentId, turn: Option<TurnId>) -> bool {
        let Some(row) = self.world.agents.get(&agent) else {
            return false;
        };
        if row.retired || row.halted || row.paused || row.open_question.is_some() {
            return false;
        }
        turn.is_none_or(|turn| {
            row.open_turn == Some(turn)
                && self
                    .world
                    .turns
                    .get(&turn)
                    .is_some_and(|turn_row| turn_row.agent == agent && !turn_row.has_ended())
                && !self.ending.contains(&turn)
        })
    }

    /// Keep the newest kind in memory. A hook's reply never waits on an activity append.
    fn observe_activity(&mut self, agent: AgentId, turn: Option<TurnId>, kind: LiveActivityKind) {
        if self.activity_sampling && self.activity_source_live(agent, turn) {
            self.activity_pending.insert(agent, (turn, kind));
        }
    }

    fn on_activity_observed(&mut self, turn: TurnId, kind: LiveActivityKind) {
        if let Some(agent) = self.world.turns.get(&turn).map(|row| row.agent) {
            self.observe_activity(agent, Some(turn), kind);
        }
    }

    /// Journal a pending kind that differs from the last journaled one, or follows a lifecycle
    /// record, at most once per [`ACTIVITY_SPACING_MS`]; or a repeat of the last journaled kind
    /// once that record is [`ACTIVITY_REFRESH_MS`] old.
    ///
    /// A held change waits for the next flush. A repeat inside the refresh window is dropped, so
    /// an agent that falls silent keeps only the lifetime its last record gave it.
    fn flush_activity(&mut self) {
        let now = self.clock.now();
        for (agent, (turn, kind)) in std::mem::take(&mut self.activity_pending) {
            if !self.activity_source_live(agent, turn) {
                continue;
            }
            let journaled = self.activity_journaled.get(&agent);
            let repeat = journaled.is_some_and(|journaled| journaled.kind == Some(kind));
            let wait = if repeat {
                ACTIVITY_REFRESH_MS
            } else {
                ACTIVITY_SPACING_MS
            };
            // A negative age is a backward clock step; waiting on it could hold samples for as
            // long as the step was.
            let due = journaled.is_none_or(|journaled| {
                let age = now.millis_since(journaled.at);
                age < 0 || age >= wait
            });
            if due {
                self.persist(Record::LiveActivityObserved { agent, kind }, Then::Fold);
            } else if !repeat {
                self.activity_pending.insert(agent, (turn, kind));
            }
        }
    }

    fn hook_activity(kind: &EventKind) -> Option<LiveActivityKind> {
        Some(match kind {
            EventKind::PromptSubmit | EventKind::PostTool { .. } | EventKind::CompactEnd => {
                LiveActivityKind::Working
            }
            EventKind::PreTool { .. } => LiveActivityKind::UsingTool,
            EventKind::CompactStart => LiveActivityKind::Compacting,
            EventKind::TurnEnd { .. } | EventKind::SessionEnd => LiveActivityKind::Idle,
            EventKind::SessionStart
            | EventKind::SubagentEnd
            | EventKind::Notification { .. }
            | EventKind::Usage { .. }
            | EventKind::HumanQuestion { .. } => return None,
        })
    }

    /// Answer a hook call, and journal anything it told us.
    ///
    /// **No hook event raises a human question**, and the absence is the subject rather than an
    /// omission. A managed agent asks by calling `agent.ask`, which arrives as
    /// `Command::Ask`; no branch journals a `Record::HumanQuestion` from
    /// `EventKind::HumanQuestion`, because its only possible feeder is a classification
    /// of the agent's output and that is refused. Leaving a second, weaker emitter beside
    /// the strong one is how the strong one gets bypassed.
    ///
    /// **The `has_breaker` filter below is not the filter [`Self::on_signal`] refuses**, and the
    /// two rules are told apart by the lifetime of the entry, not by the question they ask.
    /// `on_signal` declines to re-ask "who has a breaker" because [`Self::on_tick`] already
    /// decides it *and* clears `self.signals` at the end of every tick, so an entry keyed on an
    /// agent with no ladder is dropped on the next beat. Retirement prunes `self.compaction`, but
    /// after an unresolvable `AgentId` allocates, there is no guaranteed future retirement fold;
    /// cleanup depends on a separate later operator action. What this guard bounds is therefore
    /// which map an `AgentId` supplied on the hook wire may allocate in, and
    /// unlike a second copy of the ladder logic its absence is observable:
    /// `a_compact_start_allocates_only_for_an_agent_with_a_ladder` reads the map's size for an
    /// `AgentId` never hired and for a retired one, against a live agent as the control.
    fn on_hook(&mut self, hook: HookId, event: &Event) {
        let has_breaker = self
            .world
            .agents
            .get(&event.agent)
            .is_some_and(|row| !row.retired);
        if has_breaker {
            match &event.kind {
                EventKind::CompactStart => {
                    let now = self.clock.now();
                    self.compaction.entry(event.agent).or_default().start(now);
                }
                EventKind::CompactEnd => {
                    if let Some(compaction) = self.compaction.get_mut(&event.agent) {
                        compaction.end(self.clock.now());
                    }
                }
                _ => {}
            }
        }
        let (reply, consumed) = self.gate_reply(event.agent, &event.kind);
        if event.provenance == Provenance::Observed
            && matches!(
                reply.decision,
                HookDecision::Allow | HookDecision::Block { .. }
            )
            && let Some(kind) = Self::hook_activity(&event.kind)
        {
            self.observe_activity(event.agent, None, kind);
        }
        // a reply that carries a note is parked behind the record that spends it, so
        // a crash between the two re-delivers rather than silently having consumed it. A reply
        // that carries none is unaffected — nothing is persisted and nothing is parked.
        if let Some(consumed) = consumed {
            let record = match consumed {
                ConsumedNote::Local(steer_lsn) => {
                    self.steering.insert(event.agent, steer_lsn);
                    Record::SteerConsumed {
                        agent: event.agent,
                        steer_lsn,
                    }
                }
                ConsumedNote::Browser(note_lsn) => {
                    self.browser_context.insert(event.agent, note_lsn);
                    Record::BrowserNoteContextEmitted {
                        agent: event.agent,
                        note_lsn,
                    }
                }
            };
            self.persist(record, Then::Reply { hook, reply });
        } else {
            self.pending.push(Effect::Reply { hook, reply });
        }
    }

    /// Which notes one more `agent.steer` displaces, oldest first.
    ///
    /// **Projected depth, not acknowledged depth.** `self.world` is the fold of what has reached
    /// the disk, so a steer already persisted and not yet acknowledged is invisible to it and a
    /// clear already named is invisible too. Counting only the acknowledged queue is wrong in
    /// both directions: two steers arriving at the cap inside one fsync window would name the
    /// same front note and leave the queue one over it, and two arriving one below the cap would
    /// name nothing and leave it one over. The projection below is what `tests/steer_lifetime.rs`
    /// drives with the first persist deliberately unacknowledged.
    fn steer_evictions(&self, agent: AgentId) -> Vec<Lsn> {
        let Some(row) = self.world.agents.get(&agent) else {
            return Vec::new();
        };
        let claimed = self.steer_evicting.get(&agent);
        let appending = self.steer_appending.get(&agent).copied().unwrap_or(0);
        // A note whose `SteerConsumed` is in flight is still in `row.steers` — the world does not
        // move until the fold — but it is already spoken for, and it has to be handled **twice**
        // here or the cap goes wrong in one direction or the other.
        //
        // Counted as spent in `projected`, or the queue looks one deeper than it will be and this
        // evicts a note that did not need evicting. Excluded from the candidates, or the eviction
        // names the very position the journal is about to record as *delivered* — the same note
        // then carries a `SteerConsumed` and a `SteerCleared`, which says it was dropped
        // undelivered, and one of those records did not happen.
        //
        // Doing only the second is the subtler bug and stays green without a test for it: the
        // filter moves the eviction onto the *next* note, so a live undelivered correction is
        // dropped and the queue lands at 19. `tests/steer_lifetime.rs` asserts the depth and the
        // absence of any `SteerCleared` for exactly that reason.
        let consuming = self.steering.get(&agent).copied();
        let spent = usize::from(consuming.is_some());
        let projected =
            (row.steers.len() + appending).saturating_sub(claimed.map_or(0, BTreeSet::len) + spent);
        let over = (projected + 1).saturating_sub(MAX_PENDING_STEERS);
        row.steers
            .iter()
            .map(|(lsn, _)| *lsn)
            .filter(|lsn| !claimed.is_some_and(|claimed| claimed.contains(lsn)))
            .filter(|lsn| consuming != Some(*lsn))
            .take(over)
            .collect()
    }

    /// The note this event would carry, and where it was journaled.
    ///
    /// `None` at every kind but the two admitted ones — in particular at a stop boundary, where
    /// a note that was never consumed would otherwise be re-delivered for the life of the agent.
    /// `None` too while this agent's consumption is in flight, so the fsync window cannot deliver
    /// one note twice.
    fn pending_steer(&self, agent: AgentId, kind: &EventKind) -> Option<(Lsn, &str)> {
        if !matches!(kind, EventKind::PromptSubmit | EventKind::PostTool { .. }) {
            return None;
        }
        if self.steering.contains_key(&agent) || self.browser_context.contains_key(&agent) {
            return None;
        }
        // And none while a drop is in flight for this agent. At the cap the note the producer has
        // just named for eviction is still the front until its `SteerCleared` folds, so delivering
        // here would journal one note as both dropped **undelivered** and delivered — two records
        // asserting incompatible fates for the same position, one of which did not happen. The
        // queue would still land in the right state; the journal would not be able to say how.
        // Withholding costs a beat: the note that displaced it is delivered at the next admitted
        // event, and this guard clears as soon as the drop is on the disk.
        if self.steer_evicting.contains_key(&agent) {
            return None;
        }
        let (lsn, text) = self.world.agents.get(&agent)?.steers.front()?;
        Some((*lsn, text.as_str()))
    }

    /// The existing hook reply when it requires no durable steer consumption.
    #[must_use]
    pub fn hook_reply_without_write(&self, agent: AgentId, kind: &EventKind) -> Option<HookReply> {
        let (reply, consumed) = self.gate_reply(agent, kind);
        consumed.is_none().then_some(reply)
    }

    /// The tier-1 reply, and the position of the note it consumes when it carries one.
    ///
    /// A retired or halted agent is stopped and a paused one is refused, all without a note — so
    /// none spends a pending correction.
    fn gate_reply(&self, agent: AgentId, kind: &EventKind) -> (HookReply, Option<ConsumedNote>) {
        self.gate_reply_with_work(agent, kind, &self.work_input(agent))
    }

    /// A write-fenced hook reply using freshly composed supplemental work.
    #[must_use]
    pub fn hook_reply_without_write_with_work(
        &self,
        agent: AgentId,
        kind: &EventKind,
        work: &WorkInput,
    ) -> Option<HookReply> {
        let (reply, consumed) = self.gate_reply_with_work(agent, kind, work);
        consumed.is_none().then_some(reply)
    }

    fn gate_reply_with_work(
        &self,
        agent: AgentId,
        kind: &EventKind,
        work: &WorkInput,
    ) -> (HookReply, Option<ConsumedNote>) {
        let Some(row) = self.world.agents.get(&agent) else {
            return (HookReply::ALLOW, None);
        };
        if row.retired || row.halted {
            return (
                HookReply {
                    decision: HookDecision::Halt,
                    context: None,
                },
                None,
            );
        }
        if row.paused {
            return (
                HookReply {
                    decision: HookDecision::Deny {
                        reason: "the loop is paused".into(),
                    },
                    context: None,
                },
                None,
            );
        }
        let local = self
            .pending_steer(agent, kind)
            .map(|(lsn, text)| (lsn, text, ConsumedNote::Local(lsn)));
        let browser = (self.browser_delivery_open
            && row.attached
            && row.automatic_hold.is_none()
            && row.open_question.is_none()
            && !self.browser_context.contains_key(&agent)
            && !self.steering.contains_key(&agent)
            && matches!(kind, EventKind::PromptSubmit | EventKind::PostTool { .. }))
        .then(|| {
            self.world
                .eligible_browser_note(agent, &dudes_proto::BrowserNoteDestination::AttachedContext)
        })
        .flatten()
        .map(|(lsn, note)| (lsn, note.text.as_str(), ConsumedNote::Browser(lsn)));
        let selected = match (local, browser) {
            (Some(local), Some(browser)) => {
                Some(if local.0 <= browser.0 { local } else { browser })
            }
            (Some(note), None) | (None, Some(note)) => Some(note),
            (None, None) => None,
        };
        let (mut reply, consumed) = selected
            .map_or((HookReply::ALLOW, None), |(_, text, identity)| {
                (HookReply::allow_with(text), Some(identity))
            });
        if self.closing_active
            && matches!(kind, EventKind::PromptSubmit | EventKind::PostTool { .. })
            && let Some(session) = self
                .world
                .closing
                .as_ref()
                .filter(|session| !session.is_complete())
            && session
                .participants
                .get(&agent)
                .is_some_and(|row| row.acknowledged.is_none())
        {
            let notice = dudes_policy::closing_notice(session.announcement);
            reply.context = Some(
                reply
                    .context
                    .map_or(notice.clone(), |context| format!("{context}\n\n{notice}")),
            );
        }
        if row.attached && matches!(kind, EventKind::PromptSubmit | EventKind::PostTool { .. }) {
            let extra = dudes_policy::supplemental_context(work);
            reply.context = Some(match reply.context.take() {
                Some(text) => format!("{text}\n\n{extra}"),
                None => extra,
            });
        }
        (reply, consumed)
    }

    /// Tell every subscribed connection that an agent asked a human something.
    fn notify_question(&mut self, agent: AgentId, text: &str) {
        let params = serde_json::json!({ "agent": agent.to_string(), "text": text });
        let notifications: Vec<Effect> = self
            .clients
            .iter()
            .map(|client| Effect::Notify {
                client: *client,
                notification: Notification {
                    jsonrpc: JSONRPC.to_owned(),
                    method: "agent.question".to_owned(),
                    params: Some(params.clone()),
                },
            })
            .collect();
        self.pending.extend(notifications);
    }

    /// Tell every floor subscriber to rebuild from the now-acknowledged journal projection.
    fn notify_floor_resync(&mut self) {
        let notifications: Vec<Effect> = self
            .clients
            .iter()
            .map(|client| Effect::Notify {
                client: *client,
                notification: Notification {
                    jsonrpc: JSONRPC.to_owned(),
                    method: "floor.resync".to_owned(),
                    params: None,
                },
            })
            .collect();
        self.pending.extend(notifications);
    }
}

/// The `BudgetDecision` gate 5 refused with, or `None` when there was no refusal to journal.
///
/// a refusal is the only `BudgetDecision`. Two conditions gate it.
///
/// - **The trace carries a `Budget` step at all.** An earlier gate may have decided before
///   admission was ever checked — a halted agent whose ceiling is long past is held by gate 1 —
///   and a check that did not run is not a refusal. This is the half that rules that case out, and
///   it is the *presence* of the step that does it, not `fired`.
/// - **`admit()` says a ceiling bound.** Gate 5 fires on a breaker rung at or above `Constrain`
///   as well, where nothing was over budget. The record is written on the admission check, not on
///   the gate.
///
/// **`&& step.fired` rules out nothing the second condition does not, and is kept as a statement
/// of intent rather than as a guard.** Gate 5 consults `admit()` before the rung
/// (`dudes-policy/src/loop.rs`), so a `Budget` step that did not fire implies `admit()` allowed,
/// which the `Admission::Refused` arm already rejects. Dropping that half leaves the whole
/// workspace suite green. What makes it redundant is the ordering *inside* gate
/// 5, which is not this function's to hold — so it stays, and it would start carrying weight the
/// day the rung were checked first.
///
/// `admit()` is re-run rather than threaded out of `decide()`: it is pure and reads the same two
/// values from the same `input` gate 5 read them from, so the second call cannot disagree with the
/// first, and `Decided` stays the verdict-and-trace pair every other caller wants. The cost is one
/// more call site for `admit()`'s signature — `tests/budget_decision.rs` is what goes red if the
/// two ever diverge.
fn budget_refusal(
    agent: AgentId,
    goal: Option<GoalId>,
    input: &LoopInput,
    gate_trace: &[GateStep],
) -> Option<Record> {
    budget_refusal_values(
        agent,
        goal,
        &input.budget_state(),
        &input.ceilings(),
        input.per_turn_floor,
        gate_trace,
    )
}

fn budget_refusal_values(
    agent: AgentId,
    goal: Option<GoalId>,
    state: &dudes_policy::BudgetState,
    ceilings: &dudes_policy::Ceilings,
    reserve: dudes_proto::Spend,
    gate_trace: &[GateStep],
) -> Option<Record> {
    let fired = gate_trace
        .iter()
        .find(|step| step.gate == Gate::Budget && step.fired)?;
    let Admission::Refused {
        ceiling,
        spent,
        unknown_ends,
        per_turn_floor,
    } = admit(state, ceilings, reserve)
    else {
        return None;
    };
    Some(Record::BudgetDecision {
        agent,
        goal,
        // Permanently `false`, and the field stays on the wire anyway:
        // within frame `ver = 1` a field is never removed. It reads "this is a refusal, and it is
        // the only kind there is".
        allowed: false,
        // Observed spend, never a summed estimate.
        spent,
        unknown_ends,
        per_turn_floor,
        ceiling,
        // Gate 5's own note, so this record and `dudes why` cannot describe one refusal two ways.
        reason: fired.note.clone(),
    })
}

/// Did the agent's loop input change since the decision that is journaled for it?
///
/// The whole of the re-decide guard, in one pure function so that it can be mutated on its own:
/// folded into the sweep, the two failure directions below would only be reachable through a whole
/// tick, and the arm that matters most has no end-to-end path at all.
///
/// **Both `None`s answer "changed", and they are different `None`s.**
///
/// - `computed` is `None` when the input would not serialise. Nothing is then known about whether
///   it changed, and the direction is chosen: the comparison treats an unproducible
///   digest as changed. Deciding one extra time costs a record; skipping costs the agent. The
///   opposite reading would turn every
///   stuck agent into one that looks unchanged for ever.
/// - `stored` is `None` when the agent has never been decided for, so there is no baseline for the
///   input to be unchanged from. Unreachable from the sweep as it stands — an agent reaches
///   `Exhausted` only through a `LoopDecided`, which writes the field — and answered here anyway,
///   because the alternative is a guard whose behaviour on an empty baseline depends on which
///   caller reaches it first.
fn digest_changed(stored: Option<&str>, computed: Option<&str>) -> bool {
    match (stored, computed) {
        (_, None) | (None, Some(_)) => true,
        (Some(stored), Some(computed)) => stored != computed,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dudes_proto::{ExitStatus, Record, RecordKind, TurnOutcome};
    use dudes_types::{SessionId, TurnId};

    use super::digest_changed;
    use crate::host::HostBoot;
    use crate::input::{Command, Input};
    use crate::testing::Harness;

    #[test]
    fn predicate_effect_names_the_goals_current_holder() {
        let mut harness = Harness::new();
        let (original, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
        let current = harness.hire();
        harness.supervisor.world.apply(
            dudes_types::Lsn(99),
            &dudes_proto::Entry {
                at: dudes_types::Timestamp::UNIX_EPOCH,
                record: Record::GoalReassigned {
                    goal,
                    from: original,
                    to: current,
                    reason: None,
                },
            },
        );
        harness.supervisor.pending.clear();
        harness.supervisor.judge(original, goal);
        assert!(matches!(harness.supervisor.pending.as_slice(),
            [crate::Effect::RunPredicate { agent, .. }] if *agent == current));
    }

    /// The adapter's clean exit for one turn.
    fn exited(turn: TurnId) -> Input {
        Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        }
    }

    /// **The hand-over drain: `ending` does not grow for the life of the daemon.**
    ///
    /// [`Supervisor::ending`] holds a turn only between its end being emitted and that end being
    /// acknowledged. `apply_entry` drops the entry as it folds the `TurnEnded`, because from that
    /// moment `TurnRow::has_ended` is what refuses a second end and the run-local one has no
    /// remaining reader. Without the drain the set gains one `TurnId` per ended turn and never
    /// loses it.
    ///
    /// **Asserted in-crate because the proposition is about the set's size**, and size is not
    /// observable through `Supervisor`'s public surface at all. Every *decision* the entry
    /// influences is the same whether it is there or not — both readers are a disjunction with
    /// `has_ended()`, which the very fold that would have drained it has just made true — so no
    /// behavioural test can go red for a leak here. A memory-leak guard is still a rule, and a
    /// rule with no check reads as defended. The alternative, a public accessor for a run-local
    /// set, is API surface bought with a test.
    #[test]
    fn the_ending_set_does_not_grow_for_the_life_of_the_daemon() {
        // The control: while the end is in flight the set is what holds the turn, so the
        // assertion below is about a drain and not about an entry that was never made.
        let mut window = Harness::new();
        window.unfsynced.insert(RecordKind::TurnEnded);
        let (agent, _) = window.hire_with_goal(&["npm", "test"]);
        let turn = window.open_turn(agent).expect("a turn was issued");
        window.drive(exited(turn));
        assert!(
            window.supervisor.ending.contains(&turn),
            "a control: an end emitted and not yet acknowledged is remembered run-locally"
        );

        // The same run with the end acknowledged: the fold hands over to the row.
        let mut harness = Harness::new();
        let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
        let turn = harness.open_turn(agent).expect("a turn was issued");
        harness.drive(exited(turn));
        assert!(
            harness.supervisor.world().turns[&turn].has_ended(),
            "a control: the end folded, so the row is what refuses a second one from here on"
        );
        assert!(
            harness.supervisor.ending.is_empty(),
            "the hand-over drained the set: it does not grow for the life of the daemon"
        );
    }

    /// **The same drain, for `deferred`: it too does not grow for the life of the daemon.**
    ///
    /// The sibling of the test above, and it exists because moving that removal off `end_turn`
    /// and onto the fold is what makes it a drain at all. The once-per-cause guard has to
    /// outlive the accepted end — `tests/reaper.rs` asserts that, and that is the *first* arm
    /// here — so the only place left to drop it is the fold, and a fold that stopped dropping it
    /// would gain one `TurnId` per deferred turn and never lose one.
    ///
    /// **In-crate for the same reason**: the leak is a fact about the map's size, and after the
    /// fold no decision reads the entry — `kill_standing_cause` returns on the `open_turn` the
    /// fold cleared, and `reap` no longer sweeps a row that is not `is_open_child`. So a
    /// behavioural test cannot go red for it, and the alternative is a public accessor bought
    /// with a test.
    #[test]
    fn the_deferral_guard_does_not_grow_for_the_life_of_the_daemon() {
        // The control: the guard is what holds the turn while the end is in flight, so the
        // assertion below is about a drain and not about an entry that was never made.
        let (window, turn) = deferred_over_an_unresolvable_liveness(false);
        assert!(
            window.supervisor.deferred.contains_key(&turn),
            "a control: the standing cause's guard outlives an end that is not yet acknowledged"
        );

        // The same run with the end acknowledged: the fold takes the turn out of the sweep, and
        // the guard goes with it.
        let (harness, turn) = deferred_over_an_unresolvable_liveness(true);
        assert!(
            harness.supervisor.world().turns[&turn].has_ended(),
            "a control: the end folded, so the turn has left the reaper's sweep"
        );
        assert!(
            harness.supervisor.deferred.is_empty(),
            "the fold drained the guard: it does not grow for the life of the daemon"
        );
    }

    /// A run whose open child answers `Unknown` for ever, with a halt deferred against it and the
    /// turn's end then reported — acknowledged or held, as `fsync_the_end` says.
    ///
    /// The liveness table's diagonal is what makes the liveness unresolvable: the recorded key names no
    /// boot identity and this host produces one.
    fn deferred_over_an_unresolvable_liveness(fsync_the_end: bool) -> (Harness, TurnId) {
        let mut harness = Harness::new();
        let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
        let turn = harness.open_turn(agent).expect("a turn was issued");

        harness.host.host_boot = HostBoot::Read("boot-b".to_owned());
        harness.restart();
        if !fsync_the_end {
            harness.unfsynced.insert(RecordKind::TurnEnded);
        }

        harness.drive(Input::Command(Command::Halt {
            agent,
            text: Some("stop".into()),
        }));
        harness.drive(exited(turn));

        (harness, turn)
    }

    #[test]
    fn a_reissue_that_announced_nothing_spawns_on_no_session() {
        let spawned = spawned_under(|reissued, turn| {
            reissued.insert(turn, None);
        });
        assert!(
            spawned.issued.is_some(),
            "a control: the row holds the id its `TurnIssued` announced"
        );
        assert_eq!(
            (spawned.session, spawned.reissued),
            (None, true),
            "a re-issue that announced nothing spawns on no session, not on the row's"
        );

        let spawned = spawned_under(|_, _| {});
        assert_eq!(
            (spawned.session, spawned.reissued),
            (spawned.issued, false),
            "a control: a turn with no re-issue entry spawns on the row's session"
        );

        let minted = SessionId::from("0190f6d2-6b1a-7c3e-9f00-000000000001");
        let spawned = spawned_under(|reissued, turn| {
            reissued.insert(turn, Some(minted.clone()));
        });
        assert_eq!(
            (spawned.session, spawned.reissued),
            (Some(minted), true),
            "a control: a re-issue that announced an id spawns on that id"
        );
    }

    /// The pre-fold window with a re-issue entry that announced nothing: a session the vendor
    /// names before `TurnSpawned` folds is held and learned after it, not dropped as a second
    /// confirmation of the turn.
    #[test]
    fn a_session_named_before_a_reissue_that_announced_nothing_spawns_is_learned() {
        let (learned, agent_session) = confirmed_then_named(true);
        assert_eq!(
            (learned, agent_session),
            (
                vec![first_session(), second_session()],
                Some(second_session())
            ),
            "the re-issue keeps the session its vendor named"
        );

        let (learned, _) = confirmed_then_named(false);
        assert_eq!(
            learned,
            vec![first_session()],
            "a control: with no re-issue entry the confirmed turn refuses a second session"
        );
    }

    fn first_session() -> SessionId {
        SessionId::from("vendor-session-a")
    }

    fn second_session() -> SessionId {
        SessionId::from("vendor-session-b")
    }

    /// A turn whose launch is held, confirmed on [`first_session`], then optionally given a
    /// re-issue entry that announced nothing, then named [`second_session`] and answered. Returns
    /// the sessions it learned and the agent's session at the end.
    fn confirmed_then_named(reissued: bool) -> (Vec<SessionId>, Option<SessionId>) {
        let mut harness = Harness::new();
        harness.launch = false;
        let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
        let turn = harness.open_turn(agent).expect("a turn was issued");
        harness.drive(Input::SessionLearned {
            turn,
            session: first_session(),
        });
        if reissued {
            harness.supervisor.reissued.insert(turn, None);
        }
        harness.drive(Input::SessionLearned {
            turn,
            session: second_session(),
        });
        let key = harness.host.key_for(1000);
        harness.drive(Input::Spawned {
            turn,
            key,
            mcp_grant: None,
        });
        let learned = harness
            .records()
            .filter_map(|record| match record {
                Record::SessionLearned {
                    turn: learned,
                    session,
                    ..
                } if *learned == turn => Some(session.clone()),
                _ => None,
            })
            .collect();
        (
            learned,
            harness.supervisor.world().agents[&agent].session.clone(),
        )
    }

    struct Spawned {
        issued: Option<SessionId>,
        session: Option<SessionId>,
        reissued: bool,
    }

    fn spawned_under(
        plant: impl FnOnce(&mut BTreeMap<TurnId, Option<SessionId>>, TurnId),
    ) -> Spawned {
        let mut harness = Harness::new();
        harness.launch = false;
        let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
        let turn = harness.open_turn(agent).expect("a turn was issued");
        let issued = harness.supervisor.world().turns[&turn].session.clone();
        plant(&mut harness.supervisor.reissued, turn);
        let key = harness.host.key_for(1000);
        harness.drive(Input::Spawned {
            turn,
            key,
            mcp_grant: None,
        });
        let (session, reissued) = harness.spawned_session(turn);
        Spawned {
            issued,
            session,
            reissued,
        }
    }

    /// The comparison, all of its shapes. The two `true`s that are not an inequality are the ones
    /// the re-decide rule names, and they are asserted here rather than through a tick
    /// because no input the machine can build reaches them: `LoopInput`'s graph holds no fallible
    /// serialiser, and an agent is `Exhausted` only after a decision that stored a digest.
    #[test]
    fn an_unproducible_or_absent_digest_reads_as_changed() {
        assert!(
            !digest_changed(Some("aa"), Some("aa")),
            "the same input twice is not a change, or nothing would ever be quiet"
        );
        assert!(digest_changed(Some("aa"), Some("bb")));
        assert!(
            digest_changed(Some("aa"), None),
            "an unproducible digest fails toward deciding"
        );
        assert!(
            digest_changed(None, None),
            "and it does so whether or not there is a baseline"
        );
        assert!(
            digest_changed(None, Some("aa")),
            "never decided for is not the same as decided and unchanged"
        );
        assert!(
            !digest_changed(Some(""), Some("")),
            "an empty stored digest is compared, not interpreted: the machine journals one when \
             the input would not serialise, and two of them are still equal"
        );
    }
}

#[cfg(test)]
mod closing_controls {
    use super::*;
    use crate::closing::ClosingError;
    use crate::testing::Harness;
    use dudes_types::Tokens;

    fn ready_projection() -> (Harness, AgentId) {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.set_goal(agent, &["/usr/bin/true"]);
        let turn = harness.supervisor.world.agents[&agent].open_turn.unwrap();
        harness.supervisor.world.apply(
            Lsn(1000),
            &Entry {
                at: Timestamp::from_unix_millis(17),
                record: Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Announced,
                },
            },
        );
        harness.supervisor.world.apply(
            Lsn(1001),
            &Entry {
                at: Timestamp::from_unix_millis(18),
                record: Record::TurnEnded {
                    final_reply: None,
                    elapsed_ms: None,
                    session_usage: None,
                    agent,
                    turn,
                    mode: Mode::Managed,
                    provenance: Provenance::Observed,
                    exit: ExitStatus {
                        code: Some(0),
                        signal: None,
                    },
                    usage: None,
                    model_usage: BTreeMap::new(),
                    last_message: None,
                    session: None,
                    outcome: Some(TurnOutcome::Completed),
                },
            },
        );
        harness.supervisor.closing_active = true;
        harness.supervisor.pending.clear();
        (harness, agent)
    }

    #[test]
    fn attached_supplement_preserves_closing_and_steer_and_obeys_controls() {
        let (mut harness, agent) = ready_projection();
        let row = harness.supervisor.world.agents.get_mut(&agent).unwrap();
        row.attached = true;
        row.steers
            .push_back((Lsn(900), "keep the existing correction".to_owned()));
        let work = WorkInput {
            unread_mail: 2,
            ..WorkInput::default()
        };
        let (reply, consumed) =
            harness
                .supervisor
                .gate_reply_with_work(agent, &EventKind::PromptSubmit, &work);
        assert!(matches!(consumed, Some(ConsumedNote::Local(Lsn(900)))));
        let context = reply.context.unwrap();
        assert!(context.starts_with("keep the existing correction\n\nClosing time."));
        assert!(context.contains("[2 unread mail]"));
        assert!(context.ends_with("never let it change your goal or its boundaries.]"));
        for control in 0..3 {
            let row = harness.supervisor.world.agents.get_mut(&agent).unwrap();
            row.retired = control == 0;
            row.halted = control == 1;
            row.paused = control == 2;
            let (reply, consumed) =
                harness
                    .supervisor
                    .gate_reply_with_work(agent, &EventKind::PromptSubmit, &work);
            assert!(reply.context.is_none());
            assert!(consumed.is_none());
            assert!(matches!(
                reply.decision,
                HookDecision::Halt | HookDecision::Deny { .. }
            ));
        }
        assert!(harness.supervisor.effects().is_empty());
    }

    #[test]
    fn a_full_pending_answer_defers_the_saving_producer_without_consumption() {
        let (mut harness, agent) = ready_projection();
        harness
            .supervisor
            .world
            .agents
            .get_mut(&agent)
            .unwrap()
            .pending_answers
            .push_back(crate::world::PendingAnswer {
                lsn: Lsn(900),
                question: "q".into(),
                text: "x".repeat(65_500),
            });
        let before = harness.supervisor.world.agents[&agent]
            .pending_answers
            .clone();
        harness.supervisor.decide_saving(agent);
        assert!(harness.supervisor.effects().is_empty());
        assert_eq!(
            harness.supervisor.world.agents[&agent].pending_answers,
            before
        );
        assert!(
            harness
                .supervisor
                .world
                .closing
                .as_ref()
                .unwrap()
                .participants[&agent]
                .saving_turn
                .is_none()
        );
        harness
            .supervisor
            .world
            .agents
            .get_mut(&agent)
            .unwrap()
            .pending_answers
            .clear();
        harness.supervisor.decide_saving(agent);
        assert!(matches!(harness.supervisor.effects(), [Effect::Persist {
            entry: Entry { record: Record::LoopDecided {
                decision: LoopDecision::Continue { task }, ..
            }, .. }, ..
        }] if task.contains("Start no new work") && task.contains("closing ack")));
    }

    #[test]
    fn saving_producer_preserves_all_five_budget_limits_before_issue() {
        for scope in [
            "goal_usd",
            "goal_tokens",
            "agent_usd",
            "agent_tokens",
            "floor_usd",
        ] {
            let (mut harness, agent) = ready_projection();
            let goal = harness.supervisor.world.agents[&agent].goal.unwrap();
            match scope {
                "goal_usd" => {
                    harness
                        .supervisor
                        .world
                        .goals
                        .get_mut(&goal)
                        .unwrap()
                        .goal
                        .budget
                        .usd = Some(Usd::ZERO);
                }
                "goal_tokens" => {
                    harness
                        .supervisor
                        .world
                        .goals
                        .get_mut(&goal)
                        .unwrap()
                        .goal
                        .budget
                        .tokens = Some(Tokens::ZERO);
                }
                "agent_usd" => {
                    harness
                        .supervisor
                        .world
                        .agents
                        .get_mut(&agent)
                        .unwrap()
                        .budget
                        .usd_per_day = Some(Usd::ZERO);
                }
                "agent_tokens" => {
                    harness
                        .supervisor
                        .world
                        .agents
                        .get_mut(&agent)
                        .unwrap()
                        .budget
                        .tokens_per_day = Some(Tokens::ZERO);
                }
                "floor_usd" => harness.supervisor.config.floor_usd_per_day = Some(Usd::ZERO),
                _ => unreachable!(),
            }
            harness.supervisor.decide_saving(agent);
            assert!(
                harness.supervisor.effects().iter().any(|effect| matches!(
                    effect,
                    Effect::Persist {
                        entry: Entry {
                            record: Record::LoopDecided {
                                decision: LoopDecision::Exhausted { .. },
                                ..
                            },
                            ..
                        },
                        ..
                    }
                )),
                "{scope}"
            );
            assert!(
                !harness.supervisor.effects().iter().any(|effect| matches!(
                    effect,
                    Effect::Spawn { .. }
                        | Effect::Persist {
                            entry: Entry {
                                record: Record::TurnIssued { .. },
                                ..
                            },
                            ..
                        }
                )),
                "{scope}"
            );
        }
    }

    #[test]
    fn closing_refuses_incomplete_managed_identity_before_announcement() {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.set_goal(agent, &["/usr/bin/true"]);
        let turn = harness.supervisor.world.agents[&agent].open_turn.unwrap();
        harness
            .supervisor
            .world
            .turns
            .get_mut(&turn)
            .unwrap()
            .start_time = None;
        assert_eq!(
            harness.supervisor.closing_announce().unwrap_err(),
            ClosingError::OwnershipUnproven
        );
        assert!(harness.supervisor.world.closing.is_none());
        assert!(harness.supervisor.effects().is_empty());
        harness
            .supervisor
            .world
            .turns
            .get_mut(&turn)
            .unwrap()
            .start_time = Some(123);
        assert!(matches!(
            harness.supervisor.closing_announce().unwrap(),
            [Effect::Persist {
                entry: Entry {
                    record: Record::ClosingTime {
                        phase: dudes_proto::ClosingPhase::Announced
                    },
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn closing_refuses_durable_unspawned_work_before_announcement() {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.launch = false;
        harness.set_goal(agent, &["/usr/bin/true"]);
        harness.supervisor.spawning.clear();
        let turn = harness.supervisor.world.agents[&agent].open_turn.unwrap();
        assert!(harness.supervisor.world.turns[&turn].is_issued_not_spawned());
        assert_eq!(
            harness.supervisor.closing_announce().unwrap_err(),
            ClosingError::LaunchPending
        );
        assert!(harness.supervisor.world.closing.is_none());
        assert!(harness.supervisor.effects().is_empty());
    }

    #[test]
    fn a_recorded_saving_turn_is_not_issued_again_without_an_ack() {
        let (mut harness, agent) = ready_projection();
        harness
            .supervisor
            .world
            .closing
            .as_mut()
            .unwrap()
            .participants
            .get_mut(&agent)
            .unwrap()
            .saving_turn = Some(TurnId::from_parts(30, 31));
        harness.supervisor.decide_saving(agent);
        assert!(harness.supervisor.effects().is_empty());
        harness
            .supervisor
            .world
            .closing
            .as_mut()
            .unwrap()
            .participants
            .get_mut(&agent)
            .unwrap()
            .saving_turn = None;
        harness.supervisor.decide_saving(agent);
        assert!(matches!(
            harness.supervisor.effects(),
            [Effect::Persist {
                entry: Entry {
                    record: Record::LoopDecided {
                        decision: LoopDecision::Continue { .. },
                        ..
                    },
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn a_captured_attached_participant_does_not_gain_saving_work_after_detach() {
        let (mut harness, agent) = ready_projection();
        let mut world = World::default();
        for (position, entry) in harness.journal.iter().enumerate() {
            world.apply(Lsn(u64::try_from(position).unwrap() + 1), entry);
        }
        let turn = world.agents[&agent].open_turn.unwrap();
        world.apply(
            Lsn(1000),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::TurnEnded {
                    final_reply: None,
                    elapsed_ms: None,
                    session_usage: None,
                    agent,
                    turn,
                    mode: Mode::Managed,
                    provenance: Provenance::Observed,
                    exit: ExitStatus {
                        code: Some(0),
                        signal: None,
                    },
                    usage: None,
                    model_usage: BTreeMap::new(),
                    last_message: None,
                    session: None,
                    outcome: Some(TurnOutcome::Completed),
                },
            },
        );
        world.apply(
            Lsn(1001),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::Attached {
                    agent,
                    by: Actor::System,
                    hook_port: None,
                    provenance: None,
                },
            },
        );
        world.apply(
            Lsn(1002),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::PtySpawned {
                    agent,
                    pty: dudes_proto::rpc::pty::PtyId(9),
                    pid: 10,
                    start_time: 11,
                    boot: None,
                },
            },
        );
        world.apply(
            Lsn(1003),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Announced,
                },
            },
        );
        world.apply(
            Lsn(1004),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::Detached {
                    agent,
                    by: Actor::System,
                    provenance: None,
                },
            },
        );
        assert!(!world.agents[&agent].attached);
        assert_eq!(
            world.closing.as_ref().unwrap().participants[&agent].mode,
            Mode::Attached
        );
        harness.supervisor.world = world;
        harness.supervisor.decide_saving(agent);
        assert!(harness.supervisor.effects().is_empty());
        assert_eq!(
            harness
                .supervisor
                .world
                .closing
                .as_ref()
                .unwrap()
                .missing()
                .collect::<Vec<_>>(),
            [agent]
        );
    }

    #[test]
    fn a_captured_managed_participant_still_obeys_current_attached_control() {
        let (mut harness, agent) = ready_projection();
        harness.supervisor.world.apply(
            Lsn(1002),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::Attached {
                    agent,
                    by: Actor::System,
                    hook_port: None,
                    provenance: None,
                },
            },
        );
        assert_eq!(
            harness
                .supervisor
                .world
                .closing
                .as_ref()
                .unwrap()
                .participants[&agent]
                .mode,
            Mode::Managed
        );
        harness.supervisor.decide_saving(agent);
        assert!(matches!(
            harness.supervisor.effects(),
            [Effect::Persist {
                entry: Entry {
                    record: Record::LoopDecided {
                        decision: LoopDecision::Hold {
                            reason: dudes_proto::HoldReason::Attached
                        },
                        ..
                    },
                    ..
                },
                ..
            }]
        ));
        assert!(
            harness
                .supervisor
                .world
                .closing
                .as_ref()
                .unwrap()
                .participants[&agent]
                .saving_turn
                .is_none()
        );
    }

    /// A live, unretired agent that announce never captured — it opened no turn before the
    /// barrier — cannot ack someone else's closure, and its refusal leaves both real participants
    /// untouched.
    ///
    /// Two participants, not one: `participants.get(&agent)` and `participants.values().next()`
    /// agree on a single-entry map (the one entry is always "the first"), so a mutant that swapped
    /// the keyed lookup for positional access would pass a one-participant test. With two, the
    /// mutant would hand the bystander's call `agent_a`'s own entry — acknowledging the wrong
    /// agent instead of refusing — which is what the second assertion below catches.
    #[test]
    fn a_bystander_outside_the_snapshot_cannot_ack_someone_elses_closure() {
        let mut harness = Harness::new();
        let agent_a = harness.hire();
        harness.set_goal(agent_a, &["/usr/bin/true"]);
        let agent_b = harness.hire();
        harness.set_goal(agent_b, &["/usr/bin/true"]);
        let bystander = harness.hire();

        harness.supervisor.world.apply(
            Lsn(1000),
            &Entry {
                at: Timestamp::from_unix_millis(17),
                record: Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Announced,
                },
            },
        );
        harness.supervisor.closing_active = true;
        harness.supervisor.pending.clear();

        let captured: BTreeSet<_> = harness
            .supervisor
            .world
            .closing
            .as_ref()
            .unwrap()
            .participants
            .keys()
            .copied()
            .collect();
        assert_eq!(
            captured,
            BTreeSet::from([agent_a, agent_b]),
            "control: the bystander opened no turn, so announce did not capture it"
        );

        assert_eq!(
            harness
                .supervisor
                .closing_ack(Lsn(1000), bystander)
                .unwrap_err(),
            ClosingError::NotParticipant
        );
        let session = harness.supervisor.world.closing.as_ref().unwrap();
        assert!(
            session.participants[&agent_a].acknowledged.is_none(),
            "the refusal names no participant, so neither real one is acknowledged"
        );
        assert!(session.participants[&agent_b].acknowledged.is_none());
        assert!(harness.supervisor.effects().is_empty());
        assert!(
            !harness.records().any(|record| matches!(
                record,
                Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Ack { .. }
                }
            )),
            "a refused ack is never journaled"
        );
    }

    /// A real participant presenting a stale or otherwise wrong announcement lsn is refused
    /// before its identity is even looked up, and its own entry stays unacknowledged.
    #[test]
    fn a_participant_with_the_wrong_announcement_lsn_cannot_ack() {
        let (mut harness, agent) = ready_projection();
        let wrong = Lsn(1000 - 1);
        assert_ne!(
            wrong,
            Lsn(1000),
            "control: a different lsn than the real announcement"
        );
        assert_eq!(
            harness.supervisor.closing_ack(wrong, agent).unwrap_err(),
            ClosingError::WrongAnnouncement
        );
        assert!(
            harness
                .supervisor
                .world
                .closing
                .as_ref()
                .unwrap()
                .participants[&agent]
                .acknowledged
                .is_none()
        );
        assert!(harness.supervisor.effects().is_empty());
        assert!(!harness.records().any(|record| matches!(
            record,
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Ack { .. }
            }
        )));
    }

    /// `closing_finish` refuses before completion when this run has no active closure at all,
    /// the same `WrongAnnouncement` a caller gets from `closing_ack` for an unmatched lsn.
    #[test]
    fn closing_finish_refuses_without_an_active_closure() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.supervisor.closing_finish(false).unwrap_err(),
            ClosingError::WrongAnnouncement
        );
        assert!(harness.supervisor.effects().is_empty());
    }
}

#[cfg(test)]
mod taught_notes_controls {
    use super::*;
    use crate::testing::{Harness, Seen};
    use dudes_proto::{
        BrowserNoteAttribution, BrowserNoteDestination, ExitStatus, NoteAdmission, RecordKind,
    };

    fn completed_with_refusal() -> (Harness, AgentId, GoalId) {
        let mut h = Harness::new();
        let (agent, goal) = h.hire_with_goal(&["/usr/bin/true"]);
        let turn = h.open_turn(agent).unwrap();
        h.drive(Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(
                dudes_types::Usage::ZERO,
            )),
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        });
        h.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["/usr/bin/true".into()],
                program: Some("/usr/bin/true".into()),
                exit: Some(0),
                output_tail: String::new(),
            },
        });
        h.supervisor.handle(Input::Spend {
            agent,
            view: SpendView {
                goal: Spend {
                    usd: dudes_types::Usd::from_dollars(6),
                    ..Spend::default()
                },
                ..SpendView::default()
            },
        });
        (h, agent, goal)
    }
    fn attribution() -> BrowserNoteAttribution {
        BrowserNoteAttribution {
            request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
            request_digest: dudes_proto::record::Hex32::new([2; 32]),
            session_digest: dudes_proto::record::Hex32::new([3; 32]),
        }
    }
    #[test]
    fn taught_authority_acknowledgement_precedes_dependent_budget_refusal() {
        let (mut h, agent, goal) = completed_with_refusal();
        h.unfsynced.insert(RecordKind::TaughtNoteTurnDecided);
        let before = h.trace.len();
        h.drive(Input::Command(Command::BrowserNoteQueued {
            agent,
            text: "whole queued note".into(),
            browser: attribution(),
            destination: BrowserNoteDestination::ManagedGoal { goal },
        }));
        let emitted = &h.trace[before..];
        assert!(emitted.iter().all(|seen| !matches!(
            seen,
            Seen::Persist {
                entry: Entry {
                    record: Record::BudgetDecision { .. },
                    ..
                },
                ..
            } | Seen::Spawn { .. }
        )));
        let token = emitted
            .iter()
            .find_map(|seen| match seen {
                Seen::Persist { token, entry }
                    if matches!(entry.record, Record::TaughtNoteTurnDecided { .. }) =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .unwrap();
        let effects = h.step(Input::Persisted {
            token,
            lsn: Lsn(u64::try_from(h.journal.len()).unwrap() + 1),
        });
        assert_eq!(
            effects
                .iter()
                .filter(|seen| matches!(
                    seen,
                    Seen::Persist {
                        entry: Entry {
                            record: Record::BudgetDecision { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            effects
                .iter()
                .all(|seen| !matches!(seen, Seen::Spawn { .. }))
        );
    }
    #[test]
    fn one_unchanged_legacy_exhausted_transition_becomes_an_inert_taught_baseline() {
        let (mut h, agent, goal) = completed_with_refusal();
        let note = Lsn(10_000);
        h.supervisor.apply_entry(
            note,
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::BrowserNoteQueued {
                    agent,
                    text: "legacy queued note".into(),
                    browser: attribution(),
                    destination: BrowserNoteDestination::ManagedGoal { goal },
                },
            },
        );
        let input = h.supervisor.completed_note_input_for(agent).unwrap();
        let decided = dudes_policy::admit_completed_note(&input);
        assert!(matches!(decided.decision, NoteAdmission::Exhausted { .. }));
        h.supervisor.apply_entry(
            Lsn(10_001),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::NoteTurnDecided {
                    agent,
                    goal,
                    note_lsn: note,
                    completion_lsn: input.completion_lsn(),
                    input_digest: crate::host::completed_note_input_digest(&input).unwrap(),
                    gate_trace: decided.gate_trace,
                    decision: decided.decision,
                },
            },
        );
        let effects = h.step(Input::Tick);
        let token = effects
            .iter()
            .find_map(|effect| match effect {
                Seen::Persist { token, entry }
                    if matches!(entry.record, Record::TaughtNoteTurnDecided { .. }) =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Seen::Persist {
                        entry: Entry {
                            record: Record::TaughtNoteTurnDecided { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        h.supervisor.handle(Input::Persisted {
            token,
            lsn: Lsn(10_002),
        });
        for _ in 0..3 {
            assert!(
                h.supervisor
                    .handle(Input::Tick)
                    .iter()
                    .all(|effect| !matches!(
                        effect,
                        Effect::Persist {
                            entry: Entry {
                                record: Record::TaughtNoteTurnDecided { .. },
                                ..
                            },
                            ..
                        }
                    ))
            );
        }
        assert_eq!(
            h.supervisor.world.note_admission(agent).unwrap().contract,
            dudes_proto::NoteContract::Taught
        );
    }
}
