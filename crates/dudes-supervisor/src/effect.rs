//! What the machine asks the runtime to do.
//!
//! An effect is a *request*, never an action: `handle()` pushes these and returns; the runtime
//! executes them and feeds the results back as [`Input`](crate::Input)s. Two rules hold over the
//! whole enum:
//!
//! - **Every effect is journaled before it is acted on.** With read-only exceptions the machine emits a
//!   [`Effect::Persist`] first and the acting effect only on its acknowledgement. The read-only exceptions
//!   are [`Effect::Probe`], [`Effect::MailWakeRefresh`], [`Effect::SampleInstructions`], and
//!   [`Effect::ResolveInstructionRecoveryTicket`].
//! - **`Spawn` has one decider and one constructor.** The decider is a private `spawn()` that
//!   every path routes through — the live one, on the `Persisted` branch for the token that
//!   carried `TurnIssued`, and every re-issue. It refuses a turn whose launch this run has
//!   already asked for and not yet had answered, which is invariant 2's run-local half; the
//!   journal carries the rest. The constructor is `push_spawn()`, which `spawn()` calls and
//!   which the acknowledgement of a `HookTokenMinted` re-enters: a launch is armed with the
//!   agent's token, and under the rule above that token's record must be on the disk first.
//!   `OpenPty` has the same constructor shape in `push_open_pty()` and no
//!   decider, because nothing counts attachments.

use std::fmt;
use std::path::PathBuf;

use dudes_proto::Entry;
use dudes_proto::goal::Predicate;
use dudes_proto::reply::HookReply;
use dudes_proto::rpc::Notification;
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_types::{AgentId, GoalId, Lsn, TurnId};

use crate::host::AdoptionKey;
use crate::input::{ClientId, HookId, InstructionRequest, MailWakeRequest, PersistToken};

/// One thing the runtime must do.
///
/// Derives neither `Debug` nor `PartialEq`: `Debug` is hand-written below so that `Spawn`'s
/// `install` prints its file paths and not their contents, and equality is impossible because
/// [`AdoptionKey`] deliberately has none. The environment inside `spec` needs
/// nothing from this impl — `LaunchSpec` redacts its own.
///
/// **Deliberately not `#[non_exhaustive]`.** The runtime that executes these lives in another
/// crate, so `#[non_exhaustive]` would force it to write `_ => {}` — and a future effect that is
/// silently not executed is a turn that never spawns or a record never written. The same argument
/// keeps [`World::apply`](crate::world::World::apply)'s `Record` match exhaustive: a new variant
/// should fail to compile, loudly, at every site that must handle it. `Input` and `Command` keep
/// theirs, because an unhandled *input* is inert.
#[allow(
    clippy::large_enum_variant,
    reason = "Persist carries a 376-byte Entry after Mail metadata growth; preserve the public field type"
)]
pub enum Effect {
    /// Read current spend with one late daemon clock sample; starts no child or write.
    MailWakeRefresh {
        /// Recipient.
        agent: AgentId,
        /// Exact original goal whose cumulative spend is required.
        goal: GoalId,
        /// Volatile correlation identity.
        request: MailWakeRequest,
    },
    /// Ask the mailbox to append one exact advance owned by an acknowledged preparation.
    MailWakeNotify {
        /// Recipient.
        agent: AgentId,
        /// Acknowledged preparation identity.
        root_lsn: dudes_types::Lsn,
        /// The unique pending target and its original/next count.
        target: dudes_proto::MailWakeTarget,
    },
    /// Append this entry and acknowledge it with [`Input::Persisted`](crate::Input::Persisted)
    /// carrying the same `token`. `entry.at` is the value [`Clock::now()`](crate::Clock::now)
    /// supplied by the daemon clock at admission, retained for a joined mail decision/issue.
    Persist {
        /// The correlation id the acknowledgement must carry.
        token: PersistToken,
        /// The stamped record.
        entry: Entry,
    },
    /// Write `install`'s files if they differ, launch `spec`, and report the adoption key as
    /// [`Input::Spawned`](crate::Input::Spawned).
    Spawn {
        /// The turn the child serves.
        turn: TurnId,
        /// argv, env, cwd, stdin — typed parts, never a shell string (invariant 7).
        spec: LaunchSpec,
        /// The adapter's files, rewritten before exec: a projection, not state.
        install: ManagedInstall,
        /// The daemon-selected durable permission for this physical attempt.
        mcp_grant: Option<Lsn>,
    },
    /// Signal the child's process group. Emitted only for a turn the machine believes is
    /// `Alive`; when its liveness is `Unknown` a `Persist{KillDeferred}` goes out instead.
    Kill {
        /// The agent whose child it is.
        agent: AgentId,
        /// The turn.
        turn: TurnId,
        /// Which process.
        key: AdoptionKey,
    },
    /// Ask the process table about a recorded child and answer with
    /// [`Input::Liveness`](crate::Input::Liveness). The reaper executes this and appends nothing
    /// itself.
    ///
    /// No record precedes this read-only request.
    /// Emitted from exactly two places: `Input::Started`, for every open child, and each `Tick`, for every open child whose liveness is unresolved or whose
    /// child was adopted. A live child of the emitting run is never probed — the runtime is its
    /// parent and will report its exit, and a `Gone` racing an undelivered exit would be a second
    /// end for one turn.
    ///
    /// The runtime executes it and decides nothing: [`liveness()`](crate::host::liveness) is
    /// the liveness table, and what an answer *means* is the supervisor's alone.
    Probe {
        /// The turn.
        turn: TurnId,
        /// Which process.
        key: AdoptionKey,
    },
    /// Run the goal's predicate in `cwd` and answer with
    /// [`Input::PredicateDone`](crate::Input::PredicateDone).
    ///
    /// **The child this creates is owned under invariant 10, and by a bound shorter than either
    /// disjunct of it**. Three obligations fall on whoever executes this: the child
    /// is put in its own process group or job **at spawn**; its group is killed and reaped
    /// **before this effect's execution returns**, whether the predicate passed, failed or hit
    /// `timeout_s`; and while it is in flight it is **reachable from the daemon's exit path**,
    /// which sweeps it on all three exits in the same sequence as
    /// [`ProcessHost::shutdown`](crate::ProcessHost::shutdown). The third is the one that holds in
    /// the state the daemon is least likely to be in.
    ///
    /// **A run the exit path cut short answers nothing at all**.
    /// `on_predicate_done` stamps [`Provenance::Observed`](dudes_types::Provenance) unconditionally,
    /// so an `Input::PredicateDone { pass: false }` for a run the *daemon* interrupted would journal
    /// a fabricated `Observed` verdict — which invariant 3 forbids. The goal stays un-judged and
    /// `Done` stays unreachable until the agent's next turn ends and the machine emits this effect
    /// again. Replay does not re-issue a swept run. Un-judged is the safe direction.
    RunPredicate {
        /// The goal judged.
        goal: GoalId,
        /// The agent under judgment.
        agent: AgentId,
        /// The agent's working directory.
        cwd: PathBuf,
        /// What to run.
        predicate: Predicate,
    },
    /// Observe the recorded checkout of a ticket round's claimant before the round's first turn
    /// is issued, and answer with [`Input::BaselineCaptured`](crate::Input::BaselineCaptured).
    ///
    /// **Always answered.** A read that cannot finish answers with an unavailable baseline, not
    /// silence, because the round's first turn waits for the answer. Reads no file text into the
    /// answer and appends nothing.
    CaptureBaseline {
        /// The claimant.
        agent: AgentId,
        /// The ticket.
        card: dudes_types::CardId,
        /// The installed round.
        round: u32,
        /// The round's goal.
        goal: GoalId,
    },
    /// Sample the review scope of a ticket round after its predicate result at `check_lsn` was
    /// appended, and answer with [`Input::StampCaptured`](crate::Input::StampCaptured).
    ///
    /// **Always answered**, for the same reason as [`Effect::CaptureBaseline`]: the decision that
    /// follows the predicate result waits for the answer.
    CaptureStamp {
        /// The claimant.
        agent: AgentId,
        /// The ticket.
        card: dudes_types::CardId,
        /// The installed round.
        round: u32,
        /// The round's goal.
        goal: GoalId,
        /// Position of the baseline record being compared against.
        baseline_lsn: Lsn,
        /// That baseline.
        baseline: dudes_proto::diff_stamp::BaselinePayload,
        /// Position of the predicate result this sample follows.
        check_lsn: Lsn,
    },
    /// Answer a hook call.
    Reply {
        /// Which call.
        hook: HookId,
        /// The normalised reply; the adapter encodes it per vendor.
        reply: HookReply,
    },
    /// Send a notification to one subscribed connection.
    Notify {
        /// The connection.
        client: ClientId,
        /// The JSON-RPC notification.
        notification: Notification,
    },
    /// Write `install`'s files, then open a PTY for an attached session.
    OpenPty {
        /// The agent.
        agent: AgentId,
        /// What to run in it.
        spec: LaunchSpec,
        /// The hook settings `spec` names, written before the child starts.
        install: HookInstall,
        /// Requested terminal columns.
        cols: u16,
        /// Requested terminal rows.
        rows: u16,
    },
    /// Legacy close request with no current producer; retained for compatibility.
    ClosePty {
        /// The agent.
        agent: AgentId,
    },
    /// Resolve a browser turn's program without starting a child, then return readiness
    /// to the serialized writer through [`Input::BrowserLaunchReady`](crate::Input::BrowserLaunchReady).
    PrepareBrowserLaunch {
        /// The issued turn.
        turn: TurnId,
        /// A unique run-local preparation generation.
        generation: u64,
        /// The already armed typed launch. Preparation starts no child.
        spec: LaunchSpec,
    },
    /// Resolve current card eligibility for one historical recovery association without writing.
    ResolveInstructionRecoveryTicket {
        /// Exact volatile recovery lookup identity.
        request: InstructionRequest,
        /// Original recorded goal, card, and assignment relationship.
        association: dudes_proto::InstructionTicketAssociation,
    },
    /// Observe a starting or ending instruction boundary without launching or writing.
    SampleInstructions {
        /// Exact volatile sample identity.
        request: InstructionRequest,
        /// Actual owned attempt or attachment interval.
        owner: dudes_proto::InstructionObservationOwner,
        /// Associated immutable goal, if this work has one.
        goal: Option<GoalId>,
        /// Actual admitted model composition.
        selection: dudes_types::ModelSelection,
        /// Durable baseline for an ending comparison, absent before launch.
        baseline: Option<Box<dudes_proto::InstructionSnapshot>>,
    },
}

impl fmt::Debug for Effect {
    /// Hand-written so a `{effect:?}` in a `tracing` span or an assertion failure cannot print a
    /// secret.
    ///
    /// **The environment is not this impl's problem.** Every environment value in the workspace
    /// is an [`EnvVar`](dudes_providers::launch::EnvVar) whose `Debug` writes
    /// `"NAME"=<redacted>`, and `LaunchSpec` formats itself with that plus a `stdin_len` in place
    /// of its bytes. So `spec` is printed as it comes. The redaction travels with the value into
    /// whatever struct carries it next, rather than being re-implemented at each place one is
    /// printed. Environment names print, values do not, and argv prints in full because it is not
    /// secret.
    ///
    /// What is left here is the rest of the enum. `install` shows its file paths and not their
    /// contents: a `managed-settings.json` is a projection the daemon composes
    /// and its bytes are noise in a trace, not a secret.
    ///
    /// The secret this guards is now in this crate: every launching effect carries the agent's
    /// `DUDES_TOKEN`. `no_effect_debug_prints_the_token_plaintext` in
    /// `tests/agent_token.rs` pins this impl against a real token, with the env *key* asserted
    /// present as the control — so the test cannot pass by the environment having been dropped
    /// from the output altogether.
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive match prints every effect without its secrets"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolveInstructionRecoveryTicket {
                request,
                association,
            } => f
                .debug_struct("ResolveInstructionRecoveryTicket")
                .field("request", request)
                .field("association", association)
                .finish(),
            Self::SampleInstructions { request, owner, .. } => f
                .debug_struct("SampleInstructions")
                .field("request", request)
                .field("owner", owner)
                .finish_non_exhaustive(),
            Self::MailWakeRefresh { .. } | Self::MailWakeNotify { .. } => self.fmt_mail_wake(f),
            Self::Persist { token, entry } => f
                .debug_struct("Persist")
                .field("token", token)
                .field("entry", entry)
                .finish(),
            Self::Spawn {
                turn,
                spec,
                install,
                mcp_grant,
            } => f
                .debug_struct("Spawn")
                .field("turn", turn)
                .field("mcp_grant", mcp_grant)
                .field("spec", spec)
                .field(
                    "install",
                    &install.files.iter().map(|(p, _)| p).collect::<Vec<_>>(),
                )
                .finish(),
            Self::PrepareBrowserLaunch {
                turn,
                generation,
                spec,
            } => f
                .debug_struct("PrepareBrowserLaunch")
                .field("turn", turn)
                .field("generation", generation)
                .field("spec", spec)
                .finish(),
            Self::CaptureBaseline {
                agent,
                card,
                round,
                goal,
            } => f
                .debug_struct("CaptureBaseline")
                .field("agent", agent)
                .field("card", card)
                .field("round", round)
                .field("goal", goal)
                .finish(),
            Self::CaptureStamp {
                agent,
                card,
                round,
                goal,
                baseline_lsn,
                check_lsn,
                ..
            } => f
                .debug_struct("CaptureStamp")
                .field("agent", agent)
                .field("card", card)
                .field("round", round)
                .field("goal", goal)
                .field("baseline_lsn", baseline_lsn)
                .field("check_lsn", check_lsn)
                .finish_non_exhaustive(),
            Self::Kill { agent, turn, key } => f
                .debug_struct("Kill")
                .field("agent", agent)
                .field("turn", turn)
                .field("key", key)
                .finish(),
            Self::Probe { turn, key } => f
                .debug_struct("Probe")
                .field("turn", turn)
                .field("key", key)
                .finish(),
            Self::RunPredicate {
                goal,
                agent,
                cwd,
                predicate,
            } => f
                .debug_struct("RunPredicate")
                .field("goal", goal)
                .field("agent", agent)
                .field("cwd", cwd)
                .field("predicate", predicate)
                .finish(),
            Self::Reply { hook, reply } => f
                .debug_struct("Reply")
                .field("hook", hook)
                .field("reply", reply)
                .finish(),
            Self::Notify {
                client,
                notification,
            } => f
                .debug_struct("Notify")
                .field("client", client)
                .field("notification", notification)
                .finish(),
            Self::OpenPty {
                agent,
                spec,
                install,
                cols,
                rows,
            } => f
                .debug_struct("OpenPty")
                .field("agent", agent)
                .field("spec", spec)
                .field(
                    "install",
                    &install.files.iter().map(|(p, _)| p).collect::<Vec<_>>(),
                )
                .field("cols", cols)
                .field("rows", rows)
                .finish(),
            Self::ClosePty { agent } => f.debug_struct("ClosePty").field("agent", agent).finish(),
        }
    }
}

impl Effect {
    fn fmt_mail_wake(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MailWakeRefresh {
                agent,
                goal,
                request,
            } => f
                .debug_struct("MailWakeRefresh")
                .field("agent", agent)
                .field("goal", goal)
                .field("request", request)
                .finish(),
            Self::MailWakeNotify {
                agent,
                root_lsn,
                target,
            } => f
                .debug_struct("MailWakeNotify")
                .field("agent", agent)
                .field("root_lsn", root_lsn)
                .field("target", target)
                .finish(),
            _ => unreachable!("only mail-wake effects use this formatter"),
        }
    }
    /// The variant's name, for a trace line or an assertion message.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ResolveInstructionRecoveryTicket { .. } => "ResolveInstructionRecoveryTicket",
            Self::SampleInstructions { .. } => "SampleInstructions",
            Self::MailWakeRefresh { .. } => "MailWakeRefresh",
            Self::MailWakeNotify { .. } => "MailWakeNotify",
            Self::PrepareBrowserLaunch { .. } => "PrepareBrowserLaunch",
            Self::Persist { .. } => "Persist",
            Self::Spawn { .. } => "Spawn",
            Self::Kill { .. } => "Kill",
            Self::Probe { .. } => "Probe",
            Self::RunPredicate { .. } => "RunPredicate",
            Self::CaptureBaseline { .. } => "CaptureBaseline",
            Self::CaptureStamp { .. } => "CaptureStamp",
            Self::Reply { .. } => "Reply",
            Self::Notify { .. } => "Notify",
            Self::OpenPty { .. } => "OpenPty",
            Self::ClosePty { .. } => "ClosePty",
        }
    }

    /// The entry this effect would append, when it is a [`Effect::Persist`].
    #[must_use]
    pub const fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Persist { entry, .. } => Some(entry),
            _ => None,
        }
    }
}
