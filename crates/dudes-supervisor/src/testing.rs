//! Deterministic implementations of the three seams, plus the harness the machine's tests drive.
//!
//! Reachable from another crate's tests and from nothing else. The daemon that owns the runtime
//! tests against the same fakes, so this is not hidden behind `#[cfg(test)]` — a second copy over
//! there would be a second set of rules. But it is not part of the library either: the module is
//! declared `#[cfg(any(test, feature = "testing"))]`, so a dependent reaches it by naming the
//! `testing` feature in a `[dev-dependencies]` entry and a `cargo build -p dudes-supervisor` does
//! not compile it at all.
//!
//! Nothing here reads a clock, a process table or an entropy source. `FakeClock` returns what it
//! was told to; [`FakeRng`] counts; [`FakeHost`] answers from a script. That is what makes
//! `entry.at` assertable by value and the crash tests reproducible.
//!
//! # Why this one is gated, and it is not [`FakeHost`] that decides it
//!
//! A permissive fake of a *host* is a test that spawns nothing, so reaching for it in a daemon
//! produces an agent that never starts. That reading is right about [`FakeHost`] and says nothing
//! about [`FakeRng`], which is the fake that carries the blast radius here.
//!
//! [`FakeRng::agent_token`] is not an inert stub — it fakes the **entropy source for a
//! credential**. It formats its counter left-padded with `A` to `HOOK_TOKEN_LEN` and constructs
//! the result through the production `HookToken::new`, whose well-formedness check tests length and
//! the base64url alphabet; `A` and the digits satisfy both. So what comes out is a fully
//! well-formed agent token with no distinguishable test shape, drawn from a space — the decimal
//! integers, padded — that an attacker enumerates from 1 upward. [`crate::machine::Supervisor`] is
//! generic over [`Rng`], so before the gate existed a daemon reached that mint with one `use`
//! statement, no manifest edit and no warning: the real mint (`crate::token`, private) draws from
//! whatever `Rng` it is handed, hashes it, journals the hash, and hands the plaintext to the child.
//!
//! That plaintext is the token half of invariant 8 — the capability that separates one agent from
//! another on the socket. The failure mode is therefore not "an agent that never starts" but "an
//! agent whose token a local process guesses in one try", which is why this module is gated.
//!
//! # What the gate does not close
//!
//! The gate is the compiler: a dependent that names [`FakeRng`] without the feature fails to
//! resolve the path. `tests/no_default_fakes.rs` keeps the two conditions that live in this crate's
//! own files from being edited away, and states there what it does not scan. A dependent remains
//! free to enable the feature for a build that is not a test, and nothing outside this workspace is
//! read at all. Reaching [`FakeRng`] is now a line in a manifest rather than a `use` statement; it
//! is not impossible.
//!
//! And the gate is not what makes the shipped daemon safe — its own `Rng` is. `bins/duded`'s
//! `OsRng` reads `getrandom` and renders 32 bytes as 43 base64url characters, so the daemon on this
//! tree already mints unguessable tokens. What this feature removes is the *reach*: the next
//! daemon, or a fork, or a second binary, can no longer pick up the counted one by accident.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use dudes_policy::BreakerConfig;
use dudes_proto::goal::Predicate;
use dudes_proto::reply::HookReply;
use dudes_proto::rpc::hook::{HOOK_TOKEN_LEN, HookToken};
use dudes_proto::{AgentBudget, Entry, Goal, GoalBudget, Record, RecordKind, Spend};
use dudes_providers::adapter::{Adapter, ManagedInstall};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_types::{
    AgentId, GoalId, Lsn, Mode, Provider, SessionId, Timestamp, Tokens, TurnId, Usd,
};
use ulid::Ulid;

use crate::effect::Effect;
use crate::host::{
    AdoptionKey, Clock, HostBoot, KillOutcome, Liveness, ProbeAnswer, ProcessHost, Rng,
    SpawnResult, liveness,
};
use crate::input::{ClientId, Command, HookId, Input, PersistToken};
use crate::machine::{Config, Supervisor};

/// A clock that moves only when the test says so.
#[derive(Clone, Debug)]
pub struct FakeClock {
    at: Arc<AtomicI64>,
    step: i64,
}

impl FakeClock {
    /// A clock stuck at `millis`: every `now()` returns the same value.
    #[must_use]
    pub fn frozen(millis: i64) -> Self {
        Self {
            at: Arc::new(AtomicI64::new(millis)),
            step: 0,
        }
    }

    /// A clock that advances `step` milliseconds on each `now()`, so consecutive `Persist`
    /// effects carry distinct, predictable stamps.
    #[must_use]
    pub fn ticking(start_millis: i64, step: i64) -> Self {
        Self {
            at: Arc::new(AtomicI64::new(start_millis)),
            step,
        }
    }

    /// Move the clock forward without reading it.
    pub fn advance(&self, millis: i64) {
        self.at.fetch_add(millis, Ordering::Relaxed);
    }

    /// What the next `now()` will return.
    #[must_use]
    pub fn peek(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.at.load(Ordering::Relaxed))
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        let now = self.at.fetch_add(self.step, Ordering::Relaxed);
        Timestamp::from_unix_millis(now)
    }
}

/// A counter dressed as randomness: the nth id and the nth token are always the same bytes.
#[derive(Clone, Debug, Default)]
pub struct FakeRng {
    counter: u128,
}

impl FakeRng {
    /// A generator that has minted nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { counter: 0 }
    }

    /// How many values have been drawn.
    #[must_use]
    pub const fn drawn(&self) -> u128 {
        self.counter
    }
}

impl Rng for FakeRng {
    fn ulid(&mut self) -> Ulid {
        self.counter += 1;
        Ulid::from_parts(0, self.counter)
    }

    fn agent_token(&mut self) -> HookToken {
        self.counter += 1;
        let text = format!("{:A>width$}", self.counter, width = HOOK_TOKEN_LEN);
        HookToken::new(text).expect("a counter padded with 'A' is base64url")
    }
}

/// A process host that spawns nothing and answers liveness from a script.
///
/// The boot identity is a dimension of the fake, not of the platform: set [`Self::host_boot`] to
/// [`HostBoot::NotApplicable`] to play macOS and Windows, to [`HostBoot::Read`] to play Linux, to a
/// *different* `Read` to play a machine that rebooted, and to [`HostBoot::Unreadable`] to play a
/// Linux host whose `/proc` read failed this run — which is how the liveness table is tested
/// without a reboot, a foreign journal or a broken `/proc`.
#[derive(Clone, Debug)]
pub struct FakeHost {
    /// The boot identity this host produces now, read once at daemon start.
    ///
    /// Three-valued for the reason [`HostBoot`] gives: the two states that produce no identity
    /// have opposite consequences, and a fake that could not tell them apart could not be asked
    /// about the one that matters.
    pub host_boot: HostBoot,
    /// The start value stamped on the next key `spawn` returns.
    pub next_start_time: u64,
    /// The pid stamped on the next key `spawn` returns; incremented after each spawn.
    pub next_pid: u32,
    /// What the process table says about each pid. A pid with no entry answers
    /// [`ProbeAnswer::CouldNotAnswer`], because a fake that guesses is a fake that lies.
    pub answers: BTreeMap<u32, ProbeAnswer>,
    /// Specs handed to `spawn`, in order.
    pub spawned: Vec<(LaunchSpec, ManagedInstall)>,
    /// Pids handed out by `spawn`, in order — the fake's stand-in for the children a real host
    /// holds unreaped, and exactly the set `shutdown` sweeps. Kept after a shutdown rather than
    /// drained, so a test can ask what was swept; sweeping a pid twice is what a second
    /// `shutdown` on a host that owns nothing does anyway.
    pub spawned_pids: Vec<u32>,
    /// How many times `shutdown` was asked for — **every** call. `kill` has [`Self::killed`] for
    /// the same reason: an exit path that calls `shutdown` twice, or not at all, is the defect
    /// this counter exists to make visible.
    pub shutdowns: usize,
    /// Keys handed to `kill`, in order — **every** call, including the ones the key check
    /// refuses. A test that means "a signal was sent" asserts on the returned
    /// [`KillOutcome`]; this vector answers "the host was asked".
    pub killed: Vec<AdoptionKey>,
    /// When set, the next `spawn` fails with this kind instead of succeeding.
    pub fail_spawn: Option<io::ErrorKind>,
}

/// Every field's zero value, on a host whose platform anchors its start values at creation.
///
/// Hand-written because [`HostBoot`] has none of its own: every one of its variants is a claim
/// about a machine, so the crate that declares it refuses to nominate one as the value a caller
/// gets by saying nothing. A fake is the one place that choice can be made, and it is made here in
/// the open rather than inherited.
impl Default for FakeHost {
    fn default() -> Self {
        Self {
            host_boot: HostBoot::NotApplicable,
            next_start_time: 0,
            next_pid: 0,
            answers: BTreeMap::new(),
            spawned: Vec::new(),
            spawned_pids: Vec::new(),
            shutdowns: 0,
            killed: Vec::new(),
            fail_spawn: None,
        }
    }
}

impl FakeHost {
    /// A host with no boot identity (a macOS- or Windows-shaped host), pids from 1000.
    #[must_use]
    pub fn wall_clock_anchored() -> Self {
        Self {
            host_boot: HostBoot::NotApplicable,
            next_start_time: 900,
            next_pid: 1000,
            ..Self::default()
        }
    }

    /// A host that produces `boot` (a Linux-shaped host), pids from 1000.
    #[must_use]
    pub fn boot_relative(boot: &str) -> Self {
        Self {
            host_boot: HostBoot::Read(boot.to_owned()),
            next_start_time: 900,
            next_pid: 1000,
            ..Self::default()
        }
    }

    /// A boot-relative host whose own boot-identity read failed this run, pids from 1000.
    ///
    /// The state that is nearly unreachable: this host stamps nothing, and
    /// every key already recorded with a boot identity answers [`Liveness::Unknown`] against it
    /// for as long as the run lasts.
    #[must_use]
    pub fn boot_unreadable(reason: &str) -> Self {
        Self {
            host_boot: HostBoot::Unreadable(reason.to_owned()),
            next_start_time: 900,
            next_pid: 1000,
            ..Self::default()
        }
    }

    /// Script the process table's answer for one pid.
    pub fn answer(&mut self, pid: u32, answer: ProbeAnswer) {
        self.answers.insert(pid, answer);
    }

    /// The key a spawn of `pid` would have produced on this host.
    ///
    /// On a host whose boot-identity read failed there is no such key — [`spawn`](Self::spawn)
    /// refuses, exactly as the real host does — so what this returns for that state is the key of
    /// a spawn that cannot happen. Read it only for a host that stamps.
    #[must_use]
    pub fn key_for(&self, pid: u32) -> AdoptionKey {
        AdoptionKey {
            pid,
            start_time: self.next_start_time,
            boot: self.host_boot.as_host_boot().map(ToOwned::to_owned),
        }
    }
}

impl ProcessHost for FakeHost {
    /// Refuses on a host whose boot-identity read failed, which is the real host's rule and not a
    /// flourish: stamping `boot: None` there would assert this platform's start values are
    /// wall-clock-anchored, and that lie would survive into the journal.
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        if let Some(kind) = self.fail_spawn.take() {
            return SpawnResult::NotLaunched {
                reason: io::Error::new(kind, "FakeHost was told to fail this spawn"),
            };
        }
        if let HostBoot::Unreadable(reason) = &self.host_boot {
            return SpawnResult::NotLaunched {
                reason: io::Error::other(format!(
                    "this host's boot identity is unreadable: {reason}"
                )),
            };
        }
        self.spawned.push((spec.clone(), install.clone()));
        let key = self.key_for(self.next_pid);
        self.answers
            .insert(self.next_pid, ProbeAnswer::StartValue(self.next_start_time));
        self.spawned_pids.push(self.next_pid);
        self.next_pid += 1;
        SpawnResult::Spawned(key)
    }

    /// Applies the same logic the real host does: re-validate, then signal. The fake cannot
    /// disagree with the real one about *when* a kill is refused, because both call
    /// [`liveness()`] and neither holds a second copy of the table.
    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.killed.push(key.clone());
        match self.alive(key) {
            Liveness::Alive => KillOutcome::Signalled,
            liveness => KillOutcome::Refused { liveness },
        }
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        let probe = self
            .answers
            .get(&key.pid)
            .copied()
            .unwrap_or(ProbeAnswer::CouldNotAnswer);
        liveness(key, self.host_boot.as_host_boot(), probe)
    }

    fn boot_identity(&self) -> HostBoot {
        self.host_boot.clone()
    }

    /// Models the sweep instead of accepting it: the call is counted, and every
    /// pid this host handed out at `spawn` becomes [`ProbeAnswer::NoSuchPid`], so a later
    /// [`alive`](Self::alive) on any key `spawn` returned answers [`Liveness::Gone`]. A fake that
    /// returned `Ok(())` and changed nothing would let a daemon which never calls this pass a
    /// test that says it did — which is the one thing declaring the method on the seam buys.
    ///
    /// It sweeps **its own** children and no one else's: a pid scripted with
    /// [`answer`](Self::answer) but never spawned keeps whatever it was scripted to say. On a
    /// real host that is not a policy but a consequence — the sweep signals a reservation this
    /// host holds, so no stranger can be inside it.
    ///
    /// Cannot fail. The platform failure the trait's `# Errors` names has no analogue here, and
    /// inventing one would script an error no test asked for; a test that needs a failing
    /// shutdown should say so by adding a field, as `fail_spawn` does.
    fn shutdown(&mut self) -> io::Result<()> {
        self.shutdowns += 1;
        // Split the borrow: the sweep reads one field and writes another.
        let Self {
            answers,
            spawned_pids,
            ..
        } = self;
        for pid in &*spawned_pids {
            answers.insert(*pid, ProbeAnswer::NoSuchPid);
        }
        Ok(())
    }
}

/// One emitted effect, reduced to what a test asserts on.
///
/// `Effect` derives neither `Clone` nor `PartialEq` — `AdoptionKey` has no equality by design
/// — so the harness records this instead, and a test reads a transcript rather than
/// a borrowed slice that the next `handle()` invalidates.
#[derive(Clone, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Persist carries a 376-byte Entry after Mail metadata growth; preserve the public field type"
)]
pub enum Seen {
    /// Current-day mail admission observation request.
    MailWakeRefresh {
        /// Recipient.
        agent: AgentId,
        /// Original goal.
        goal: GoalId,
        /// Volatile correlation.
        request: crate::input::MailWakeRequest,
    },
    /// One exact mailbox advance request.
    MailWakeNotify {
        /// Recipient.
        agent: AgentId,
        /// Acknowledged preparation.
        root_lsn: Lsn,
        /// Original receipt and counts.
        target: dudes_proto::MailWakeTarget,
    },
    /// An entry the machine wants written.
    Persist {
        /// The ledger's correlation id.
        token: PersistToken,
        /// The stamped record.
        entry: Entry,
    },
    /// A launch. Invariant 2's subject.
    Spawn {
        /// The turn.
        turn: TurnId,
        /// argv, env, cwd, stdin.
        spec: LaunchSpec,
        /// The adapter's files, to be written before exec.
        install: ManagedInstall,
        /// The durable managed-launch permission, absent before an agent's first edit.
        mcp_grant: Option<Lsn>,
    },
    /// A signal to a child.
    Kill {
        /// The agent.
        agent: AgentId,
        /// The turn.
        turn: TurnId,
    },
    /// A question for the process table.
    Probe {
        /// The turn.
        turn: TurnId,
    },
    /// A predicate run.
    RunPredicate {
        /// The goal judged.
        goal: GoalId,
    },
    /// A hook answer.
    Reply {
        /// Which call.
        hook: HookId,
        /// What was said.
        reply: HookReply,
    },
    /// A notification to a subscriber.
    Notify {
        /// The connection.
        client: ClientId,
        /// The JSON-RPC method name.
        method: String,
    },
    /// A PTY opened.
    OpenPty {
        /// The agent.
        agent: AgentId,
        /// argv, env, cwd, stdin — carried for the same reason [`Seen::Spawn`] carries it: an
        /// attached child is armed with the agent's token exactly as a managed one is,
        /// and a reduction that dropped the spec would make that half of the
        /// rule unassertable.
        spec: LaunchSpec,
    },
    /// A PTY closed.
    ClosePty {
        /// The agent.
        agent: AgentId,
    },
    /// A revocable browser launch preparation that starts no child.
    PrepareBrowserLaunch {
        /// The turn.
        turn: TurnId,
        /// Unique run-local attempt generation.
        generation: u64,
        /// Typed launch parts.
        spec: LaunchSpec,
    },
    /// Read current canonical ticket eligibility for one pending instruction recovery.
    ResolveInstructionRecoveryTicket {
        /// Volatile lookup identity.
        request: crate::input::InstructionRequest,
        /// Original historical ticket association.
        association: dudes_proto::InstructionTicketAssociation,
    },
    /// A bounded observation request; the pure harness has no filesystem sampler.
    SampleInstructions {
        /// Run-local correlation.
        request: crate::input::InstructionRequest,
        /// Recorded physical owner.
        owner: dudes_proto::InstructionObservationOwner,
        /// Associated goal.
        goal: Option<GoalId>,
        /// Frozen ending baseline.
        baseline: Option<Box<dudes_proto::InstructionSnapshot>>,
    },
}

impl Seen {
    /// Reduce one effect.
    #[must_use]
    pub fn of(effect: &Effect) -> Self {
        match effect {
            Effect::ResolveInstructionRecoveryTicket {
                request,
                association,
            } => Self::ResolveInstructionRecoveryTicket {
                request: *request,
                association: *association,
            },
            Effect::SampleInstructions {
                request,
                owner,
                goal,
                baseline,
                ..
            } => Self::SampleInstructions {
                request: *request,
                owner: *owner,
                goal: *goal,
                baseline: baseline.clone(),
            },
            Effect::MailWakeRefresh {
                agent,
                goal,
                request,
            } => Self::MailWakeRefresh {
                agent: *agent,
                goal: *goal,
                request: *request,
            },
            Effect::MailWakeNotify {
                agent,
                root_lsn,
                target,
            } => Self::MailWakeNotify {
                agent: *agent,
                root_lsn: *root_lsn,
                target: target.clone(),
            },
            Effect::PrepareBrowserLaunch {
                turn,
                generation,
                spec,
            } => Self::PrepareBrowserLaunch {
                turn: *turn,
                generation: *generation,
                spec: spec.clone(),
            },
            Effect::Persist { token, entry } => Self::Persist {
                token: *token,
                entry: entry.clone(),
            },
            Effect::Spawn {
                turn,
                spec,
                install,
                mcp_grant,
            } => Self::Spawn {
                turn: *turn,
                spec: spec.clone(),
                install: install.clone(),
                mcp_grant: *mcp_grant,
            },
            Effect::Kill { agent, turn, .. } => Self::Kill {
                agent: *agent,
                turn: *turn,
            },
            Effect::Probe { turn, .. } => Self::Probe { turn: *turn },
            Effect::RunPredicate { goal, .. } => Self::RunPredicate { goal: *goal },
            Effect::Reply { hook, reply } => Self::Reply {
                hook: *hook,
                reply: reply.clone(),
            },
            Effect::Notify {
                client,
                notification,
            } => Self::Notify {
                client: *client,
                method: notification.method.clone(),
            },
            Effect::OpenPty { agent, spec, .. } => Self::OpenPty {
                agent: *agent,
                spec: spec.clone(),
            },
            Effect::ClosePty { agent } => Self::ClosePty { agent: *agent },
        }
    }

    /// The record inside a [`Seen::Persist`].
    #[must_use]
    pub const fn record(&self) -> Option<&Record> {
        match self {
            Self::Persist { entry, .. } => Some(&entry.record),
            _ => None,
        }
    }

    /// The variant's name, for an assertion message.
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
            Self::Reply { .. } => "Reply",
            Self::Notify { .. } => "Notify",
            Self::OpenPty { .. } => "OpenPty",
            Self::ClosePty { .. } => "ClosePty",
        }
    }
}

/// A machine, a fake host, and the loop that closes between them.
///
/// [`Harness::drive`] runs an input to quiescence: each emitted effect is executed against the
/// [`FakeHost`] and its result fed back, until nothing more is produced. The three flags open the
/// windows a crash test needs — turn `fsync` off and every `Persist` goes unacknowledged, which is
/// exactly the gap invariant 2 is about.
pub struct Harness {
    /// The machine under test.
    pub supervisor: Supervisor<FakeClock, FakeRng>,
    /// The process host it acts through.
    pub host: FakeHost,
    /// Entries that reached the disk, in `lsn` order: the journal a restart replays.
    pub journal: Vec<Entry>,
    /// Every effect emitted since the harness was built, in order.
    pub trace: Vec<Seen>,
    /// When false, no `Persist` is ever acknowledged: the crash window, wide open.
    pub fsync: bool,
    /// Record kinds whose `Persist` is never acknowledged: the same window, aimed at one record,
    /// so a test can lose exactly the `TurnIssued` and keep everything before it.
    pub unfsynced: BTreeSet<RecordKind>,
    /// When false, a `Spawn` is never executed: the other crash window.
    pub launch: bool,
    /// When false, a `Probe` is never answered.
    pub probe: bool,
    next_lsn: u64,
    /// How this harness builds the adapter its floor runs on. A factory rather than the adapter
    /// itself because [`Harness::restart`] builds a second machine and has to give it the same
    /// floor — an adapter moved into the first `Supervisor` could not be handed to the second,
    /// and a restart that silently reverted to the Claude adapter would be a fake that lies.
    /// `Send` is part of the type, not an accident: `Harness` was `Send` before this field existed, and
    /// a boxed closure without the bound silently removed it — `auto_trait_impl_removed`, a major break
    /// for a harness nobody meant to change. It is asserted below rather than left to the next release.
    adapter: Box<dyn Fn() -> Box<dyn Adapter> + Send>,
    /// The configuration this harness's machine runs under, held for the same reason as
    /// [`Harness::adapter`]: [`Harness::restart`] builds a second machine and must give it the
    /// same one. A restart that silently reverted to [`Harness::config`]'s floor would be a fake
    /// that lies — a test whose subject is an absent ceiling would pass on a $100 one.
    config: Config,
}

impl Harness {
    /// A harness over a Claude-driven floor, a wall-clock-anchored host, and a clock that
    /// advances one millisecond per read so every entry carries a distinct, predictable `at`.
    #[must_use]
    pub fn new() -> Self {
        Self::with(
            FakeClock::ticking(1_787_394_600_000, 1),
            FakeHost::wall_clock_anchored(),
        )
    }

    /// A harness over an explicit clock and host, on the Claude adapter.
    #[must_use]
    pub fn with(clock: FakeClock, host: FakeHost) -> Self {
        Self::with_adapter(clock, host, || {
            Box::new(ClaudeAdapter::new(ClaudeConfig::default()))
        })
    }

    /// A harness whose floor runs on `adapter` instead of the Claude one.
    ///
    /// For a test whose subject is what the machine **hands** an adapter rather than what an
    /// adapter does with it. Two of [`AgentCtx`](dudes_providers::adapter::AgentCtx)'s fields
    /// reach no shipped adapter at all: `ClaudeAdapter` is the workspace's only `impl Adapter`
    /// outside a test, and it reads `agent_dir`, `cwd`, `daemon_env`, `trusted`, `max_turns` and
    /// `max_budget` — never `agent`, never `sandbox`. No `LaunchSpec` can witness those two, so
    /// only the context itself can.
    ///
    /// `adapter` is called once per machine, which is once here and once per
    /// [`restart`](Self::restart); a recorder therefore shares its log across the two rather than
    /// starting the second run blind.
    #[must_use]
    pub fn with_adapter(
        clock: FakeClock,
        host: FakeHost,
        adapter: impl Fn() -> Box<dyn Adapter> + Send + 'static,
    ) -> Self {
        Self::with_config(Self::config(), clock, host, adapter)
    }

    /// A harness over an explicit [`Config`], for a test whose subject is something
    /// [`Harness::config`] fixes.
    ///
    /// The one that exists for is the **absent floor ceiling**: `config()` pins
    /// `floor_usd_per_day: Some($100)` so that gate 5 is never the reason a test fails, which is
    /// right for every arm whose subject is something else — and makes an arm whose subject is
    /// *no ceiling anywhere* unwritable, because the floor is set and no constructor took a
    /// config. Build one with struct-update syntax over `config()` and hand it here.
    ///
    /// Additive on purpose: [`Harness::with_adapter`] delegates to this with `config()`, so every
    /// caller that does not care is unchanged and `config()` stays the default.
    #[must_use]
    pub fn with_config(
        config: Config,
        clock: FakeClock,
        host: FakeHost,
        adapter: impl Fn() -> Box<dyn Adapter> + Send + 'static,
    ) -> Self {
        Self {
            supervisor: Self::machine(config.clone(), clock, adapter()),
            host,
            journal: Vec::new(),
            trace: Vec::new(),
            fsync: true,
            unfsynced: BTreeSet::new(),
            launch: true,
            probe: true,
            next_lsn: 0,
            adapter: Box::new(adapter),
            config,
        }
    }

    /// The configuration every harness runs under: one data directory, one socket, a two-entry
    /// daemon environment, and a floor ceiling high enough that gate 5 is never the reason.
    #[must_use]
    pub fn config() -> Config {
        Config {
            data_dir: PathBuf::from("/var/lib/dudes"),
            socket: PathBuf::from("/var/lib/dudes/dudes.sock"),
            daemon_env: vec![EnvVar::new("PATH", "/usr/bin")],
            floor_usd_per_day: Some(Usd::from_dollars(100)),
            per_turn_floor: Spend {
                usd: Usd::from_micros(1),
                tokens: Tokens::new(1),
                work_tokens: Tokens::new(1),
            },
            breaker: BreakerConfig::default(),
            // The attach gate refuses a path that is not absolute on the host platform.
            hook_command: Some(PathBuf::from(if cfg!(windows) {
                r"C:\dudes\bin\dudes-hook.exe"
            } else {
                "/opt/dudes/bin/dudes-hook"
            })),
            agent_bin: None,
            mcp: dudes_providers::mcp::LoadedServers::default(),
        }
    }

    fn machine(
        config: Config,
        clock: FakeClock,
        adapter: Box<dyn Adapter>,
    ) -> Supervisor<FakeClock, FakeRng> {
        Supervisor::new(config, clock, FakeRng::new())
            .with_adapters([adapter].into_iter().collect())
    }

    /// A second daemon run over the entries that reached the disk: a fresh machine, the journal
    /// replayed through `Input::Replayed`, then `Input::Started`.
    ///
    /// The fresh `FakeRng` is the point — a restart mints new ids and a new agent token, exactly
    /// as a real one does, so nothing a test asserts about re-issue can come from remembered
    /// state.
    pub fn restart(&mut self) {
        let entries = self.journal.clone();
        self.next_lsn = u64::try_from(entries.len()).expect("the harness prefix fits an LSN");
        self.supervisor = Self::machine(
            self.config.clone(),
            FakeClock::ticking(1_787_394_700_000, 1),
            (self.adapter)(),
        );
        // The harness journal is the log: `follow_up` pushes an entry and hands out the next
        // `Lsn` in the same breath, so its first entry was acknowledged at `Lsn(1)` and the
        // n-th at `Lsn(n)`. Replaying at those same positions is what makes this second run a
        // replay of the first rather than a differently-numbered one.
        for (entry, position) in entries.into_iter().zip(1u64..) {
            let _ = self.supervisor.handle(Input::Replayed {
                lsn: Lsn(position),
                entry,
            });
        }
        self.drive(Input::Started);
    }

    /// [`Harness::restart`], with the daemon having found `missing` agents' worktrees gone before
    /// `Input::Started`.
    pub fn restart_with_missing_worktrees(&mut self, missing: Vec<AgentId>) {
        let entries = self.journal.clone();
        self.next_lsn = u64::try_from(entries.len()).expect("the harness prefix fits an LSN");
        self.supervisor = Self::machine(
            self.config.clone(),
            FakeClock::ticking(1_787_394_700_000, 1),
            (self.adapter)(),
        );
        for (entry, position) in entries.into_iter().zip(1u64..) {
            let _ = self.supervisor.handle(Input::Replayed {
                lsn: Lsn(position),
                entry,
            });
        }
        self.drive(Input::MissingWorktrees(missing));
        self.drive(Input::Started);
    }

    /// One `handle()` call. The effects go into [`Harness::trace`] and nothing is answered.
    pub fn step(&mut self, input: Input) -> Vec<Seen> {
        let seen: Vec<Seen> = self.supervisor.handle(input).iter().map(Seen::of).collect();
        for record in seen.iter().filter_map(Seen::record) {
            assert_refusal(record);
        }
        self.trace.extend(seen.iter().cloned());
        seen
    }

    /// Run an input to quiescence, executing every effect the flags allow.
    pub fn drive(&mut self, input: Input) {
        let mut queue = VecDeque::from([input]);
        while let Some(input) = queue.pop_front() {
            for seen in self.step(input) {
                if let Some(next) = self.follow_up(seen) {
                    queue.push_back(next);
                }
            }
        }
    }

    fn follow_up(&mut self, seen: Seen) -> Option<Input> {
        match seen {
            Seen::ResolveInstructionRecoveryTicket {
                request,
                association,
            } => Some(Input::InstructionRecoveryTicketResolved {
                request,
                ticket: Some(association),
            }),
            Seen::SampleInstructions {
                request,
                owner: _,
                goal,
                baseline,
            } => Some(self.mock_instruction_sample(request, goal, baseline)),
            Seen::PrepareBrowserLaunch {
                turn,
                generation,
                mut spec,
            } if self.launch => {
                spec.argv[0] = std::env::temp_dir()
                    .join("prepared-managed-engine")
                    .into_os_string();
                Some(Input::BrowserLaunchReady {
                    turn,
                    generation,
                    prepared: Ok(Box::new(spec)),
                })
            }
            Seen::Persist { token, entry }
                if self.fsync && !self.unfsynced.contains(&entry.record.kind()) =>
            {
                self.journal.push(entry);
                self.next_lsn += 1;
                Some(Input::Persisted {
                    token,
                    lsn: Lsn(self.next_lsn),
                })
            }
            Seen::Spawn {
                turn,
                spec,
                install,
                mcp_grant,
            } if self.launch
                && self.supervisor.claim_browser_dispatch(
                    turn,
                    self.trace.iter().rev().find_map(|seen| {
                        if let Seen::PrepareBrowserLaunch {
                            turn: prepared,
                            generation,
                            ..
                        } = seen
                        {
                            (*prepared == turn).then_some(*generation)
                        } else {
                            None
                        }
                    }),
                ) =>
            {
                match self.host.spawn(&spec, &install) {
                    SpawnResult::Spawned(key) => Some(Input::Spawned {
                        turn,
                        key,
                        mcp_grant,
                    }),
                    SpawnResult::NotLaunched { reason } => Some(Input::SpawnFailed {
                        turn,
                        reason: reason.to_string(),
                    }),
                    SpawnResult::CleanupUnconfirmed { .. } => None,
                }
            }
            Seen::Probe { turn } if self.probe => {
                let key = self.key_of(turn)?;
                Some(Input::Liveness {
                    turn,
                    liveness: self.host.alive(&key),
                })
            }
            _ => None,
        }
    }

    fn mock_instruction_sample(
        &self,
        request: crate::input::InstructionRequest,
        goal: Option<GoalId>,
        baseline: Option<Box<dudes_proto::InstructionSnapshot>>,
    ) -> Input {
        let gap = dudes_proto::InstructionCoverageGap {
            reason: dudes_proto::InstructionGapKind::DeclarationMissing,
            context: dudes_proto::InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        };
        let snapshot = dudes_proto::InstructionSnapshot {
            sampled_at: Timestamp::UNIX_EPOCH,
            source: dudes_proto::InstructionSnapshotSource::Fresh,
            execution_folder: None,
            participants: Vec::new(),
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: vec![
                dudes_proto::InstructionCoverageGap {
                    reason: dudes_proto::InstructionGapKind::PathUnresolved,
                    ..gap.clone()
                },
                gap.clone(),
            ],
            usage: dudes_proto::InstructionSampleUsage::default(),
            boot: None,
        };
        let ticket = goal.and_then(|goal| {
            self.supervisor
                .world()
                .instructions
                .association_for_goal(goal, dudes_types::Seq::ZERO)
        });
        Input::InstructionSampled {
            request,
            snapshot: Box::new(snapshot),
            ticket,
            comparison: baseline.map(|_| {
                Box::new(dudes_proto::InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![gap],
                    usage: dudes_proto::InstructionSampleUsage::default(),
                })
            }),
        }
    }

    /// The adoption key the journal recorded for a turn.
    ///
    /// Delegates rather than rebuilding: a second derivation here could disagree with the one the
    /// machine's probes carry, and a fake that answers about a different key from the one under
    /// test proves nothing.
    #[must_use]
    pub fn key_of(&self, turn: TurnId) -> Option<AdoptionKey> {
        self.supervisor.world().turns.get(&turn)?.adoption_key()
    }

    /// A reader observation attributed to this fixture's recorded physical child.
    #[must_use]
    pub fn session_observation(
        &self,
        turn: TurnId,
        observation: dudes_providers::stream::SessionObservation,
    ) -> Input {
        Input::SessionObserved {
            turn,
            key: self.key_of(turn).expect("a recorded physical child"),
            mcp_grant: self.supervisor.world().turns[&turn].mcp_grant,
            observation,
        }
    }

    /// Every `Seen::Spawn` for one turn. Invariant 2's assertion is a count over this.
    #[must_use]
    pub fn spawns_of(&self, turn: TurnId) -> Vec<&Seen> {
        self.trace
            .iter()
            .filter(|seen| matches!(seen, Seen::Spawn { turn: t, .. } if *t == turn))
            .collect()
    }

    /// Every record the machine asked to have written, in order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.trace.iter().filter_map(Seen::record)
    }

    /// `TurnSpawned`'s `(session, reissued)` for a turn: what it was *launched* on.
    #[must_use]
    pub fn spawned_session(&self, turn: TurnId) -> (Option<SessionId>, bool) {
        self.records()
            .find_map(|record| match record {
                Record::TurnSpawned {
                    turn: spawned,
                    session,
                    reissued,
                    ..
                } if *spawned == turn => Some((session.clone(), *reissued)),
                _ => None,
            })
            .expect("the spawn was recorded")
    }

    /// How many agents hold a run-local compaction entry.
    #[must_use]
    pub fn compaction_entries(&self) -> usize {
        self.supervisor.test_compaction_entries()
    }

    /// Whether this run still owns the launch claim for `turn`.
    #[must_use]
    pub fn is_spawning(&self, turn: TurnId) -> bool {
        self.supervisor.test_is_spawning(turn)
    }

    /// Override the compaction suppression ceiling in a test fixture.
    ///
    /// Production exposes no corresponding configuration or wire surface. This exists for the
    /// discriminating control that raising the ceiling by one delays the breaker cap by one beat.
    pub fn set_compaction_max_suppressed_beats(&mut self, beats: u8) {
        self.supervisor
            .set_test_compaction_max_suppressed_beats(beats);
    }

    /// Hire an agent and set it a goal whose predicate is `argv`, then run to quiescence.
    ///
    /// Returns the pair of ids the machine minted. The commonest two commands in one call,
    /// because a test about invariant 2 should not spend twenty lines getting to a turn. Setting
    /// the goal decides, so by the time this returns a turn is in flight — unless a flag says
    /// otherwise.
    pub fn hire_with_goal(&mut self, argv: &[&str]) -> (AgentId, GoalId) {
        let agent = self.hire();
        let goal = self.set_goal(agent, argv);
        (agent, goal)
    }

    /// `agent.hire`, run to quiescence. An agent without a goal is never decided for, so nothing
    /// else happens.
    pub fn hire(&mut self) -> AgentId {
        self.drive(Input::Command(Command::Hire {
            character: None,
            accent: None,
            name: "Hector".into(),
            role: "infra".into(),
            department: "infra".into(),
            provider: Provider::Claude,
            cwd: PathBuf::from("/repo"),
            workspace: None,
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            // `None`, and a test about the recorded caps must not take them from here: a harness
            // that supplied a positive value would make the launch site's `row.max_turns` look
            // right even if it were replaced by that same constant. `tests/agent_ctx.rs` drives
            // `Command::Hire` itself for exactly that reason.
            max_turns: None,
            max_budget: None,
            trusted: false,
            worktree: None,
            mcp_servers: Vec::new(),
        }));
        *self
            .supervisor
            .world()
            .agents
            .keys()
            .next_back()
            .expect("the hire was journaled")
    }

    /// `goal.set`, run to quiescence. This is what starts the loop.
    pub fn set_goal(&mut self, agent: AgentId, argv: &[&str]) -> GoalId {
        self.drive(Input::Command(Command::GoalSet {
            completion_check: None,
            agent,
            text: "make the tests pass".into(),
            predicate: Predicate::Command {
                argv: argv.iter().map(|part| (*part).to_owned()).collect(),
                timeout_s: 600,
            },
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 8,
            browser_goal: None,
            goal_replacement: None,
            baseline: None,
        }));
        *self
            .supervisor
            .world()
            .goals
            .keys()
            .next_back()
            .expect("the goal was journaled")
    }

    /// The turn the agent has in flight, when it has one.
    #[must_use]
    pub fn open_turn(&self, agent: AgentId) -> Option<TurnId> {
        self.supervisor.world().agents.get(&agent)?.open_turn
    }
}

/// Distinct, ordered stamps for a hand-built journal.
fn journal_at(step: i64) -> Timestamp {
    Timestamp::from_unix_millis(1_787_394_600_000 + step)
}

/// A journal holding one agent, one goal, and one turn that was issued with `session` and `task`
/// and never spawned: the shape [`Harness::hire_with_goal`] writes, for a replay to re-issue.
pub fn journal_with_issued_turn(
    session: Option<SessionId>,
    task: &str,
) -> (Vec<Entry>, AgentId, TurnId) {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid");
    let goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("a ulid");
    let turn = TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("a ulid");
    let entries = vec![
        Entry {
            at: journal_at(0),
            record: Record::AgentHired {
                character: None,
                accent: None,
                id: agent,
                name: "Hector".to_owned(),
                role: "infra".to_owned(),
                department: "infra".to_owned(),
                provider: Provider::Claude,
                cwd: std::path::PathBuf::from("/repo"),
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(10)),
                    tokens_per_day: None,
                },
                sandbox: None,
                isolation: None,
                max_turns: None,
                max_budget: None,
                trusted: false,
                workspace: None,
                worktree: None,
                mcp_servers: Vec::new(),
                browser_onboarding: None,
            },
        },
        Entry {
            at: journal_at(1),
            record: Record::GoalSet {
                agent,
                goal: Goal {
                    completion_check: None,
                    id: goal,
                    text: task.to_owned(),
                    predicate: Predicate::Command {
                        argv: vec!["npm".to_owned(), "test".to_owned()],
                        timeout_s: 600,
                    },
                    budget: GoalBudget {
                        usd: Some(Usd::from_dollars(5)),
                        tokens: None,
                    },
                    hop_cap: 8,
                    baseline: None,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        },
        Entry {
            at: journal_at(2),
            record: Record::TurnIssued {
                agent,
                turn,
                goal,
                task: task.to_owned(),
                session,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn(1),
                answers: Vec::new(),
            },
        },
    ];
    (entries, agent, turn)
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// a refusal is the only `BudgetDecision`, so every one ever written carries
/// `allowed: false`.
///
/// **Asserted here rather than at the one call site, and that placement is the check.** Under
/// the rule that payload fields are only ever added within frame `ver = 1`, the `allowed` field can never be removed, so
/// what keeps "permanently false" true is not the writer's own test — a later writer would come
/// with its own — but a rule every record in this crate's suite passes through. `Harness::step`
/// sees every effect every test produces, which makes reintroducing the admitted case red on
/// whatever test reintroduces it rather than only on the one that looked for it.
///
/// `bins/duded`'s `Fixture::entries` carries the same assertion over the journal on disk; between
/// them the two cover the machine's output and the daemon's.
fn assert_refusal(record: &Record) {
    if let Record::BudgetDecision { allowed, .. } = record {
        assert!(
            !allowed,
            "a BudgetDecision is journaled on a refusal and never on an admission, \
             so `allowed` is permanently false: {record:?}"
        );
    }
}

/// `Harness` is `Send`, and this is the check that says so rather than a release noticing.
///
/// A bare `Box<dyn Fn()>` adapter factory would remove the auto impl, which `cargo semver-checks`
/// reports as `auto_trait_impl_removed`, a major break. An auto trait is a property of the
/// fields, so it moves whenever a field does.
///
/// This is a compile-time assertion: with the `+ Send` bound removed from the factory field it
/// fails to build, which is a failure the next author meets in seconds rather than in CI.
const _: fn() = || {
    const fn assert_send<T: Send>() {}
    assert_send::<Harness>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn first_loaded_prefix_append(missing_worktrees: bool) -> (Lsn, Lsn) {
        let (entries, _, _) = journal_with_issued_turn(None, "make the tests pass");
        let tail = Lsn(u64::try_from(entries.len()).unwrap());
        let mut harness = Harness::new();
        harness.journal = entries;
        harness.fsync = false;
        if missing_worktrees {
            harness.restart_with_missing_worktrees(Vec::new());
        } else {
            harness.restart();
        }
        let first = harness
            .trace
            .iter()
            .find(|seen| matches!(seen, Seen::Persist { .. }))
            .cloned()
            .expect("recovery requests an exact missing-boundary append");
        harness.fsync = true;
        let Some(Input::Persisted { lsn, .. }) = harness.follow_up(first) else {
            panic!("the mock writer acknowledges its first appended frame");
        };
        (tail, lsn)
    }

    #[test]
    fn a_loaded_prefix_restart_appends_after_its_replayed_tail() {
        let (tail, appended) = first_loaded_prefix_append(false);
        assert_eq!(appended, Lsn(tail.0 + 1));
    }

    #[test]
    fn a_loaded_prefix_worktree_restart_appends_after_its_replayed_tail() {
        let (tail, appended) = first_loaded_prefix_append(true);
        assert_eq!(appended, Lsn(tail.0 + 1));
    }

    /// The clock is the only source of time, so a frozen one stamps every entry alike and a
    /// ticking one gives each `Persist` its own value.
    #[test]
    fn the_fake_clock_is_frozen_or_ticks_by_a_known_step() {
        let frozen = FakeClock::frozen(10);
        assert_eq!(frozen.now(), Timestamp::from_unix_millis(10));
        assert_eq!(frozen.now(), Timestamp::from_unix_millis(10));
        frozen.advance(5);
        assert_eq!(frozen.now(), Timestamp::from_unix_millis(15));

        let ticking = FakeClock::ticking(100, 7);
        assert_eq!(ticking.now(), Timestamp::from_unix_millis(100));
        assert_eq!(ticking.now(), Timestamp::from_unix_millis(107));
        assert_eq!(ticking.peek(), Timestamp::from_unix_millis(114));
    }

    /// Tokens out of the seam are well-formed `HookToken`s and never repeat, so a test can tell
    /// one daemon run's mint from the next.
    #[test]
    fn fake_tokens_are_well_formed_and_distinct() {
        let mut rng = FakeRng::new();
        let first = rng.agent_token();
        let second = rng.agent_token();
        assert_eq!(first.expose().len(), HOOK_TOKEN_LEN);
        assert_ne!(first, second);
        assert_ne!(rng.ulid(), rng.ulid());
        assert_eq!(rng.drawn(), 4);
    }

    /// A pid the script says nothing about answers `CouldNotAnswer`, so a test that forgot to
    /// script one sees `Unknown` — never an accidental `Gone`.
    #[test]
    fn an_unscripted_pid_is_unknown() {
        let host = FakeHost::wall_clock_anchored();
        assert_eq!(host.alive(&host.key_for(7)), Liveness::Unknown);
    }
}
