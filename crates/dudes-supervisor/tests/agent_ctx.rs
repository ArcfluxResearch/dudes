//! What the machine puts in an `AgentCtx`, asserted by value on the route that builds one.
//!
//! `Supervisor::launch` holds the workspace's only `AgentCtx` construction, and it is the last hop
//! before every managed spawn: whatever it writes into those eight fields is what the vendor CLI
//! is launched with. Without this file, **six of the seven fields
//! were holes** — `agent` set to a constant id, `cwd` to `/tmp`, `agent_dir` to `&row.cwd`,
//! `sandbox` to `Some(true)`, `max_turns` to `Some(3)`, `max_budget` to `Some(1)`, each on its own,
//! left the workspace suite green.
//!
//! `daemon_env` was the exception and is worth naming, because "all seven" would be the easier and
//! wronger sentence. Setting it to `&[]` is already red elsewhere: it kills
//! `duded::managed_turn a_managed_turn_ends_with_the_usage_its_stream_reported`, whose
//! `HelperAdapter` copies `ctx.daemon_env` into the child's environment and whose helper reads its
//! role from there. That kill is an assertion rather than a spawn that never came up — it lands in
//! 2.5 s against the same test's usual 0.7 s, not the 10.4 s of a launch failing to start.
//!
//! **A `LaunchSpec` cannot close this, which is why the assertion is on the context.** An adapter
//! is the only thing that turns a context into an observable launch, and `ClaudeAdapter` is the
//! workspace's only `impl Adapter` outside a test file. It composes its launch from six of the
//! eight fields — `agent_dir`, `cwd`, `daemon_env`, `trusted`, `max_turns`, `max_budget` — and reads
//! neither `agent` nor `sandbox`; `sandbox_flag` returns `None` for it because Claude's on-switch is a
//! settings key rather than a flag. So `sandbox` is unwitnessable downstream by construction
//! rather than by omission, and asserting on spawned argv would leave exactly the field the sandbox refusal's
//! mutation arm foundered on still free.
//!
//! The recorder below is an adapter that reads the context and keeps a copy, so the context itself
//! becomes the observation. It is registered as `Provider::Claude` because that is what the
//! harness hires on; it replaces the Claude adapter rather than wrapping it, so nothing here
//! depends on what Claude's argv happens to look like.
//!
//! **Two agents, and be exact about what that buys.** **Four** of the eight expected values differ
//! between the two rows — `agent`, `cwd`, `agent_dir`, `sandbox` — and those four are the ones no
//! constant can satisfy: a fixed `cwd` is wrong for at least one agent, `Some(true)` is wrong for
//! the agent hired `Some(false)`, and a transposed field (`cwd` where `agent_dir` belongs) is wrong
//! for both.
//!
//! The other four are pinned differently and more weakly, which is worth saying rather than
//! rounding up. `max_turns` and `max_budget` are `None` in the two rows of the test below because
//! those hires ask for no cap, and what that pins is only that no cap appears the hire did not ask
//! for. They *do* have a source on `AgentRow`, and the opposite polarity — a
//! recorded cap reaching the first launch and the re-issue alike — is
//! `recorded_run_limits_reach_the_first_launch_and_the_reissue` below, exactly as the recorded-true
//! `trusted` test is the opposite polarity for its own field. `daemon_env` is the same value in
//! both rows, so a constant *equal to* `Harness::config`'s environment — a daemon that stopped
//! reading its own config — passes here; what catches that is the `duded` test named above, not
//! this one. This file's `daemon_env` assertion earns its place against `&[]` and a transposition,
//! and no further. `trusted` is false in both of these live hires; the recorded-true first/reissue
//! test below is its opposite-polarity control.
//!
//! **The `sandbox` rows are a legacy-replay state, deliberately.** `agent.hire` refuses both `Some(true)` and `Some(false)` before `Control::hire`, so `None` is the only value a *hire* can journal
//! today and a `Some(_)` on an `AgentRow` comes from replaying a journal written before the field existed.
//! Replay is a legal supervisor input and the machine must build a context for it, which is why
//! the rows below drive `Command::Hire` into the machine directly rather than through the refusing
//! handler. If a later change normalises `sandbox` at hire, this test should be updated with it —
//! it is not asserting that an operator can still ask for one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dudes_policy::input::{AgentInput, Control, FloorInput, GoalInput, LoopInput, WorkInput};
use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{AgentBudget, Entry, GoalBudget, Record, Spend};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::mcp::McpLaunch;
use dudes_providers::stream::TurnReader;
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness};
use dudes_supervisor::{Command, Input, input_digest};
use dudes_types::{AgentId, BreakerRung, ModelSelection, Provider, Tier, Timestamp, Usd};

/// One `AgentCtx`, copied out of the borrow the machine built it as.
///
/// The context borrows three of its fields from the agent row, the daemon's config and a local
/// `PathBuf`, none of which outlive the call; a test that asserts after the fact needs the values,
/// so this owns them. Field for field with `AgentCtx` and no wider, and [`Built::of`]'s
/// destructuring bind is what holds that true: a tenth field on `AgentCtx` stops this file
/// compiling rather than quietly falling out of what it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Built {
    agent: AgentId,
    cwd: PathBuf,
    agent_dir: PathBuf,
    daemon_env: Vec<EnvVar>,
    sandbox: Option<bool>,
    trusted: bool,
    max_turns: Option<u32>,
    max_budget: Option<Usd>,
    mcp_servers: Vec<McpLaunch>,
}

impl Built {
    fn of(ctx: &AgentCtx<'_>) -> Self {
        // Destructured rather than read field by field, and that is the enforcement of the
        // paragraph above. Accessor reads let a tenth field on `AgentCtx` be added, set at the
        // construction site and never asserted here, with the suite green and nothing to notice;
        // this pattern makes the same edit `error[E0027]: pattern does not mention field` in this
        // file. The `..` rest pattern would give the silence back, so it is deliberately absent.
        let AgentCtx {
            agent,
            cwd,
            agent_dir,
            daemon_env,
            sandbox,
            trusted,
            max_turns,
            max_budget,
            mcp_servers,
        } = ctx;
        Self {
            agent: *agent,
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            daemon_env: daemon_env.to_vec(),
            sandbox: *sandbox,
            trusted: *trusted,
            max_turns: *max_turns,
            max_budget: *max_budget,
            mcp_servers: mcp_servers.to_vec(),
        }
    }
}

/// An adapter that answers every managed launch identically and keeps the context it was handed.
///
/// Pure in the sense the trait means: the launch it returns is a function of nothing at all, so
/// two calls with equal inputs give equal bytes. The log is shared rather than owned because the
/// harness takes the adapter by value and a restart builds a second one.
#[derive(Clone, Default)]
struct Recorder {
    built: Arc<Mutex<Vec<Built>>>,
}

impl Recorder {
    fn seen(&self) -> Vec<Built> {
        self.built
            .lock()
            .expect("no test panics while holding the log")
            .clone()
    }
}

impl Adapter for Recorder {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        _session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        self.built
            .lock()
            .expect("no test panics while holding the log")
            .push(Built::of(ctx));
        Ok(LaunchSpec {
            argv: vec!["true".into()],
            env: Vec::new(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::None
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// No variable bills a turn on this stand-in (the empty answer).
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// `agent.hire` with a chosen repository and sandbox switch, run to quiescence.
///
/// `Harness::hire` fixes both, and both are the subject here.
fn hire(harness: &mut Harness, cwd: &str, sandbox: Option<bool>) -> AgentId {
    hire_with_limits(harness, cwd, sandbox, None, None)
}

/// The same hire, with the two recorded provider-run limits chosen by the caller.
///
/// Separate from `Harness::hire` on purpose: that helper hardcodes `None` for both, so a test
/// that took its limits from it could not tell `row.max_turns` at the launch site apart from a
/// literal `None` written there.
fn hire_with_limits(
    harness: &mut Harness,
    cwd: &str,
    sandbox: Option<bool>,
    max_turns: Option<u32>,
    max_budget: Option<Usd>,
) -> AgentId {
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from(cwd),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox,
        isolation: None,
        max_turns,
        max_budget,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
    }));
    *harness
        .supervisor
        .world()
        .agents
        .keys()
        .next_back()
        .expect("the hire was journaled")
}

/// The recorded run limits: the two recorded provider-run limits reach the launch context on the first
/// issue and on the crash re-issue, and a configuration change between the two cannot move them.
///
/// The same shape as `hire_trust_comes_from_the_record_not_the_config` below, and for the same
/// reason: the re-issue is a second daemon run rebuilding the context from the replayed journal,
/// so an implementation that read the current `[providers]` table instead of `AgentRow` would
/// differ here and nowhere else. `Built` equality covers both fields at once because it is a
/// whole-struct comparison; the `assert!` above it names them so a failure says which.
#[test]
fn recorded_run_limits_reach_the_first_launch_and_the_reissue() {
    let recorder = Recorder::default();
    let log = recorder.clone();
    let mut harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || Box::new(recorder.clone()),
    );

    let capped = hire_with_limits(
        &mut harness,
        "/repo/capped",
        None,
        Some(7),
        Some(Usd::from_micros(2_500_000)),
    );
    harness.launch = false;
    harness.set_goal(capped, &["npm", "test"]);
    harness.launch = true;
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent: capped,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(capped)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let seen = log.seen();
    assert_eq!(
        seen.len(),
        2,
        "one first issue and one crash re-issue: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|ctx| ctx.max_turns == Some(7)
                && ctx.max_budget == Some(Usd::from_micros(2_500_000))),
        "both contexts carry the recorded limits: {seen:?}"
    );
    assert_eq!(
        seen[0], seen[1],
        "the re-issue rebuilds the same context from the journal rather than from current \
         configuration"
    );
}

/// The recorded run limits: neither recorded limit is an input to a decision.
///
/// # What is compared, and why it is bytes
///
/// `LoopDecided.input_digest` is blake3 over the canonical serialisation of the whole
/// `dudes_policy::LoopInput`, so two agents whose digests are equal agreed on **every** field
/// `decide()` reads — the three admission ceilings and the hop count among them — and not merely
/// on the four this test could think to name. `Supervisor::input_digest` is the same function the
/// machine journals with, so the comparison is over the real bytes rather than a restatement.
///
/// # The two halves, and why they are built differently
///
/// **The subject runs through the machine**, because the claim is about a value with no
/// `LoopInput` field at all: `max_turns` and `max_budget` cannot be *varied* on a `LoopInput`, so
/// the only way to show they do not reach one is to hire agents that differ in nothing else and
/// watch the digest stay put. An implementation that folded either into a ceiling — clamping the
/// agent's daily dollars to the backstop, say — moves the digest and reddens this half.
///
/// **The discriminating control is built at the source level**, one `LoopInput` per input, and it
/// is not an optional flourish: an `input_digest` that ignored its argument, or a constant
/// `Some(String::new())`, passes the first half and every agent in the workspace. Each row here
/// changes exactly one of the four real admission inputs — the goal ceiling, the
/// agent ceiling, the floor ceiling, `hop_cap` — and each must move the digest on its own. The
/// floor is the row that *has* to be built this way rather than hired: it is daemon configuration
/// (`Config::floor_usd_per_day`), fixed for a whole harness, so no pair of hires can differ in it.
///
/// The control is evaluated **first**, so a failing subject cannot leave it unrun.
#[test]
fn neither_recorded_limit_reaches_the_decide_input() {
    let base = LoopInput {
        control: Control::default(),
        last_turn_end: None,
        open_question: None,
        goal: GoalInput {
            text: "make the tests pass".to_owned(),
            hop_cap: 8,
            turns_on_goal: 0,
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            spent: Spend::default(),
            unknown_ends: 0,
            predicate: None,
            baseline: None,
        },
        agent: AgentInput {
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            spent_today: Spend::default(),
            unknown_ends_today: 0,
            rung: BreakerRung::Healthy,
        },
        floor: FloorInput {
            usd_per_day: Some(Usd::from_dollars(100)),
            spent_today: Spend::default(),
            unknown_ends_today: 0,
        },
        per_turn_floor: Spend::default(),
        work: WorkInput::default(),
        answers: Vec::new(),
    };
    let digest_of = |input: &LoopInput| input_digest(input).expect("the input serialises");
    let baseline = digest_of(&base);

    let mut goal_ceiling = base.clone();
    goal_ceiling.goal.budget.usd = Some(Usd::from_dollars(6));
    let mut agent_ceiling = base.clone();
    agent_ceiling.agent.budget.usd_per_day = Some(Usd::from_dollars(11));
    let mut floor_ceiling = base.clone();
    floor_ceiling.floor.usd_per_day = Some(Usd::from_dollars(101));
    let mut hops = base.clone();
    hops.goal.hop_cap = 9;
    for (what, changed) in [
        ("the goal ceiling", goal_ceiling),
        ("the agent ceiling", agent_ceiling),
        ("the floor ceiling", floor_ceiling),
        ("the hop cap", hops),
    ] {
        assert_ne!(
            digest_of(&changed),
            baseline,
            "{what} is a real admission input: moving it must move the digest, or the equalities \
             below prove nothing"
        );
    }

    let recorder = Recorder::default();
    let mut harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || Box::new(recorder.clone()),
    );
    let digest_after_hire = |harness: &mut Harness, limits: (Option<u32>, Option<Usd>)| {
        let agent = hire_with_limits(harness, "/repo/same", None, limits.0, limits.1);
        harness.set_goal(agent, &["npm", "test"]);
        harness
            .supervisor
            .world()
            .agents
            .get(&agent)
            .expect("the hire was journaled")
            .last_input_digest
            .clone()
            .expect("the goal decided, so a digest was journaled")
    };

    let uncapped = digest_after_hire(&mut harness, (None, None));
    let turns_capped = digest_after_hire(&mut harness, (Some(7), None));
    let budget_capped = digest_after_hire(&mut harness, (None, Some(Usd::from_micros(2_500_000))));
    assert_eq!(
        (turns_capped.as_str(), budget_capped.as_str()),
        (uncapped.as_str(), uncapped.as_str()),
        "a recorded provider-run limit is a backstop inside the child, not an admission input: \
         three agents alike in everything else must decide against byte-equal input"
    );
}

/// Changing `cwd: &row.cwd` at `machine.rs`'s `AgentCtx` construction to a fixed
/// path makes `cargo nextest run --workspace` go red.
///
/// Every field is pinned, not only that one. The eight `assert_eq!`-worth of expectation is one
/// comparison over a two-element vector so that the count is asserted too: a context built twice
/// for one turn, or never, fails here rather than passing a per-field loop over an empty log.
///
/// The expected `agent_dir` is spelled out from `Harness::config`'s data directory rather than
/// asked of the machine. Asking would be a tautology — `agent_dir()` is the
/// function under test on that field, and comparing it with itself would hold for any body it
/// could be given.
#[test]
fn every_field_of_the_launch_context_comes_from_the_agent_it_is_built_for() {
    let recorder = Recorder::default();
    let log = recorder.clone();
    // Neither the clock nor the host is load-bearing here: nothing below reads a timestamp or a
    // pid. They are the defaults `Harness::new` uses, named because the adapter arm needs them.
    let mut harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || Box::new(recorder.clone()),
    );

    let alpha = hire(&mut harness, "/repo/alpha", Some(true));
    harness.set_goal(alpha, &["npm", "test"]);
    let beta = hire(&mut harness, "/repo/beta", Some(false));
    harness.set_goal(beta, &["npm", "test"]);

    assert_ne!(alpha, beta, "a control: the two hires are distinct agents");

    // Not a count but a correspondence, and the difference is a live hole. `launch()` returns the
    // adapter's `LaunchSpec` one line below the `AgentCtx` it is built from, and a context that is
    // right about `cwd` does not make the spec that leaves the function right about it: inserting
    // `spec.cwd = PathBuf::from("/tmp")` after the `AgentCtx` is built — a correct context whose result is
    // overwritten in the same call — leaves the context assertion
    // below still green. Reading the cwds the *host* was handed is what closes the last hop, and it
    // is not a restatement of the context: `Recorder::managed` copies `ctx.cwd` into the spec, so
    // these two assertions differ exactly over what happens between the adapter's return and the
    // spawn.
    let launched: Vec<PathBuf> = harness
        .host
        .spawned
        .iter()
        .map(|(spec, _)| spec.cwd.clone())
        .collect();
    assert_eq!(
        launched,
        vec![PathBuf::from("/repo/alpha"), PathBuf::from("/repo/beta")],
        "each context reached the host as the launch of the agent it was built for: two spawns, \
         each in its own agent's repository"
    );

    let gamma = hire(&mut harness, "/repo/gamma", None);
    harness.launch = false;
    harness.set_goal(gamma, &["npm", "test"]);
    harness.launch = true;
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent: gamma,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(gamma)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let config = Harness::config();
    let expected = |agent: AgentId, cwd: &str, sandbox: Option<bool>| Built {
        agent,
        cwd: PathBuf::from(cwd),
        agent_dir: config.data_dir.join("agents").join(agent.to_string()),
        daemon_env: config.daemon_env.clone(),
        sandbox,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: Vec::new(),
    };
    let for_gamma = expected(gamma, "/repo/gamma", None);

    assert_eq!(
        log.seen(),
        vec![
            expected(alpha, "/repo/alpha", Some(true)),
            expected(beta, "/repo/beta", Some(false)),
            for_gamma.clone(),
            for_gamma,
        ],
        "the launch context is the agent's own — its repository, its directory, its sandbox \
         switch, and no per-turn cap the hire did not ask for — and a re-issue after a restart \
         rebuilds the same one from the replayed journal"
    );
}

/// repository trust comes from the durable hire and survives replay. The first
/// launch and the crash re-issue must therefore receive the same `true`, without consulting a
/// current adapter setting that could reinterpret the journal.
#[test]
fn hire_trust_comes_from_the_record_not_the_config() {
    let recorder = Recorder::default();
    let log = recorder.clone();
    let mut harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || Box::new(recorder.clone()),
    );
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
    let hired = Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::AgentHired {
            character: None,
            accent: None,
            id: agent,
            name: "Trusted".to_owned(),
            role: "engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: PathBuf::from("/repo/trusted"),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: true,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        },
    };
    harness.append_replayed(hired);

    harness.launch = false;
    harness.set_goal(agent, &["npm", "test"]);
    harness.launch = true;
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let seen = log.seen();
    assert_eq!(
        seen.len(),
        2,
        "one first issue and one crash re-issue: {seen:?}"
    );
    assert!(
        seen.iter().all(|ctx| ctx.agent == agent && ctx.trusted),
        "both contexts use the recorded trust: {seen:?}"
    );
    assert_eq!(
        seen[0], seen[1],
        "replay rebuilds the same context instead of reading current trust configuration"
    );
}
