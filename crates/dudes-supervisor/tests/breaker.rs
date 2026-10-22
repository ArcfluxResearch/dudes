//! The 30-second beat: signals collected between ticks, the ladder moved once per tick, and the
//! two things a rung is *for* — gate 5 refusing the next turn, and `Stop` ending the running one.
//!
//! `dudes_policy::beat()` was tested from the day it was written and nothing called it, so the
//! rung stayed `Healthy` for the life of a daemon and gate 5's breaker clause could not fire.
//! Every test here drives the machine to reach the rung; none constructs one.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use dudes_policy::{
    COMPACTION_GRACE_MS, COMPACTION_MAX_SUPPRESSED_BEATS, COMPACTION_TRAILING_GRACE_MS,
};
use dudes_proto::goal::Evidence;
use dudes_proto::{
    AgentBudget, Entry, Event, EventKind, ExhaustedReason, GoalBudget, KillReason, LoopDecision,
    Record, TurnOutcome,
};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{LaunchError, LaunchSpec};
use dudes_providers::stream::{Credential, SessionObservation, TurnReader};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_supervisor::{Command, Config, HookId, Input, RuntimeSignal};
use dudes_types::{
    AgentId, BreakerRung, BreakerSignal, IsolationMode, Lsn, ModelSelection, Provenance, Provider,
    Tier, Timestamp, Tokens, TurnStatus, Usd,
};

/// Replay one record at `position`, the way the log hands it over: the entry carries no position
/// of its own, so the caller supplies the one the frame sat at.
fn replay(harness: &mut Harness, position: u64, at: Timestamp, record: Record) {
    assert_eq!(harness.append_replayed(Entry { at, record }), Lsn(position));
}

/// Every `BreakerBeat` the machine asked to have written, oldest first.
fn beats(harness: &Harness) -> Vec<(BreakerRung, BreakerRung, Vec<BreakerSignal>)> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::BreakerBeat {
                rung_before,
                rung_after,
                signals,
                ..
            } => Some((*rung_before, *rung_after, signals.clone())),
            _ => None,
        })
        .collect()
}

/// The rung the fold currently holds for one agent — what gate 5 reads.
fn rung(harness: &Harness, agent: AgentId) -> BreakerRung {
    harness.supervisor.world().agents[&agent].rung
}

/// The decision on the most recent `LoopDecided`, with the note of the gate that fired.
fn last_decision(harness: &Harness) -> (LoopDecision, String) {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LoopDecided {
                decision,
                gate_trace,
                ..
            } => Some((
                decision.clone(),
                gate_trace
                    .last()
                    .and_then(|step| step.note.clone())
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .last()
        .expect("the machine decided at least once")
}

struct ConfigDirAdapter(ClaudeAdapter);

impl Adapter for ConfigDirAdapter {
    fn provider(&self) -> Provider {
        self.0.provider()
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        self.0.tier()
    }

    fn isolation(&self) -> Isolation {
        Isolation::ConfigDir
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        self.0.session_acquisition()
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        self.0.turn_reader()
    }

    fn model_selection(&self) -> ModelSelection {
        self.0.model_selection()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError> {
        self.0.managed(ctx, task, session, selection)
    }

    fn managed_install(&self, ctx: &AgentCtx<'_>, selection: &ModelSelection) -> ManagedInstall {
        self.0.managed_install(ctx, selection)
    }

    fn usage_source(&self) -> UsageSource {
        self.0.usage_source()
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        self.0.sandbox_flag()
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        self.0.billing_env_vars()
    }
}

fn assert_server_leak_persists_before_notifying_and_never_stops() {
    let mut leaking = Harness::new();
    leaking.drive(Input::Command(Command::Subscribe {
        client: dudes_supervisor::ClientId(1),
    }));
    let (agent, _) = leaking.hire_with_goal(&["npm", "test"]);
    let turn = leaking.open_turn(agent).expect("a turn is running");
    leaking.fsync = false;

    let first = leaking.step(leaking.session_observation(
        turn,
        SessionObservation {
            servers: vec!["repository-tools".into()],
            ..SessionObservation::default()
        },
    ));
    let (token, beat) = first
        .iter()
        .find_map(|seen| match seen {
            Seen::Persist { token, entry } => Some((*token, &entry.record)),
            _ => None,
        })
        .expect("the leak first asks for one durable beat");
    assert!(matches!(
        beat,
        Record::BreakerBeat {
            rung_before: BreakerRung::Healthy,
            rung_after: BreakerRung::Steer,
            signals,
            ..
        } if signals == &[BreakerSignal::IsolationLeak]
    ));
    assert_eq!(
        first.len(),
        1,
        "the floor is not notified before the beat is acknowledged"
    );

    let before_ack = leaking.trace.len();
    leaking.drive(leaking.persisted(token, Lsn(10_000)));
    assert!(matches!(
        &leaking.trace[before_ack..],
        [Seen::Notify { method, .. }] if method == "floor.resync"
    ));
    assert_eq!(beats(&leaking).len(), 1, "one init produces one beat");
    assert_eq!(rung(&leaking, agent), BreakerRung::Steer);
    assert!(
        leaking
            .trace
            .iter()
            .all(|seen| !matches!(seen, Seen::Kill { agent: killed, .. } if *killed == agent)),
        "one isolation leak never stops the running child"
    );
}

fn assert_empty_and_inherited_inits_do_not_leak() {
    let mut empty = Harness::new();
    let (empty_agent, _) = empty.hire_with_goal(&["npm", "test"]);
    let empty_turn = empty.open_turn(empty_agent).expect("control turn");
    empty.drive(empty.session_observation(empty_turn, SessionObservation::default()));
    assert!(beats(&empty).is_empty(), "an empty init is not a leak");

    let mut inherited = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || {
            Box::new(ClaudeAdapter::new(ClaudeConfig {
                isolation: IsolationMode::Inherit,
                ..ClaudeConfig::default()
            }))
        },
    );
    let inherited_agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
    replay(
        &mut inherited,
        1,
        Timestamp::from_unix_millis(1_787_394_600_000),
        Record::AgentHired {
            character: None,
            accent: None,
            id: inherited_agent,
            name: "Trusted".to_owned(),
            role: "engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: "/repo/trusted".into(),
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
    );
    inherited.set_goal(inherited_agent, &["npm", "test"]);
    let inherited_turn = inherited
        .open_turn(inherited_agent)
        .expect("inherit control turn");
    inherited.drive(inherited.session_observation(
        inherited_turn,
        SessionObservation {
            plugins: vec![Some("memory".into())],
            ..SessionObservation::default()
        },
    ));
    assert!(
        beats(&inherited).is_empty(),
        "Isolation::None accepts inherited plugins"
    );
}

fn assert_plugins_leak_for_flags_and_config_dir() {
    let mut plugin_only = Harness::new();
    let (plugin_agent, _) = plugin_only.hire_with_goal(&["npm", "test"]);
    let plugin_turn = plugin_only.open_turn(plugin_agent).expect("plugin turn");
    plugin_only.drive(plugin_only.session_observation(
        plugin_turn,
        SessionObservation {
            plugins: vec![Some("memory".into())],
            ..SessionObservation::default()
        },
    ));
    assert_eq!(
        beats(&plugin_only),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            vec![BreakerSignal::IsolationLeak]
        )],
        "plugins alone prove a leak under flag isolation"
    );

    let mut config_dir = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || {
            Box::new(ConfigDirAdapter(
                ClaudeAdapter::new(ClaudeConfig::default()),
            ))
        },
    );
    let (config_agent, _) = config_dir.hire_with_goal(&["npm", "test"]);
    let config_turn = config_dir.open_turn(config_agent).expect("config-dir turn");
    config_dir.drive(config_dir.session_observation(
        config_turn,
        SessionObservation {
            plugins: vec![Some("memory".into())],
            ..SessionObservation::default()
        },
    ));
    assert_eq!(
        beats(&config_dir),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            vec![BreakerSignal::IsolationLeak]
        )],
        "ConfigDir has the same runtime proof as Flags"
    );
}

/// the observed init payload is the runtime proof for managed-turn isolation.
///
/// The persisted beat is the public seam: it names the leak, advances exactly one rung and lands
/// before the floor is told to refresh. The empty init and `Isolation::None` arms are live
/// controls against treating every init as a leak. Finally, the lone observed leak leaves the
/// breaker below `Stop` and emits no kill, preserving invariant 3's non-terminal first beat.
#[test]
fn isolation_leak_init_emits_one_beat_and_never_stops() {
    assert_server_leak_persists_before_notifying_and_never_stops();
    assert_empty_and_inherited_inits_do_not_leak();
    assert_plugins_leak_for_flags_and_config_dir();
}

/// The beats one Claude turn's init produces: the captured `s1-first` init, with `plant` added to
/// its plugins, through the Claude reader.
fn claude_init_beats(
    plant: Option<serde_json::Value>,
) -> Vec<(BreakerRung, BreakerRung, Vec<BreakerSignal>)> {
    let captured = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../dudes-providers/tests/fixtures/claude/s1-first.stdout"),
    )
    .expect("the captured run");
    let mut init: serde_json::Value =
        serde_json::from_str(captured.lines().next().expect("a first line")).unwrap();
    assert_eq!(init["subtype"], "init");
    if let Some(plugin) = plant {
        init["plugins"].as_array_mut().unwrap().push(plugin);
    }
    let mut reader: Box<dyn TurnReader> =
        Box::new(dudes_providers::claude::stream::TurnStream::new());
    let observation = reader
        .line(&init.to_string())
        .session
        .expect("the init opens the session");

    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn is running");
    harness.drive(harness.session_observation(turn, observation));
    beats(&harness)
}

/// Claude Code 2.1.283 lists `agents-md@builtin` and `telemetry@builtin` in every init with no
/// plugin configured. They ship inside the CLI, so they are no leak. A plugin installed from a
/// marketplace still is, and so is one from a marketplace named `builtin`.
#[test]
fn claude_built_in_plugins_are_no_leak_and_an_installed_plugin_is() {
    assert!(
        claude_init_beats(None).is_empty(),
        "the vendor's built-in plugins are not a leak"
    );
    for marketplace in ["team", "builtin"] {
        let planted = serde_json::json!({
            "name": "memory",
            "path": format!("/tmp/cfg/.claude/plugins/cache/{marketplace}/memory/1.0.0"),
            "source": format!("memory@{marketplace}"),
        });
        assert_eq!(
            claude_init_beats(Some(planted)),
            [(
                BreakerRung::Healthy,
                BreakerRung::Steer,
                vec![BreakerSignal::IsolationLeak]
            )],
            "a plugin from the {marketplace} marketplace is a leak"
        );
    }
}

// ---- uncapped billing: an uncapped billing source raises a rung ----------------------------------------

/// A harness with **no ceiling anywhere**: no floor, no agent budget, no goal budget.
///
/// Every one of the three has to be absent, and each is absent for its own reason — which is why
/// this helper exists rather than a `Harness::new()` and one override. `Harness::config()` pins
/// `floor_usd_per_day: Some($100)` so gate 5 is never the reason an unrelated test fails, and
/// `Harness::hire` / `set_goal` pin `$10` and `$5`; an arm whose subject is *no ceiling* has to
/// unset all three or it measures nothing.
///
/// The ceiling-less hire and goal are driven as commands rather than replayed, and **the
/// supervisor admits both**: the `ceiling_required` refusal lives at the daemon's
/// door (the daemon's `ceiling_required` check), reads the call's own budget and the daemon
/// environment, and never reaches this fold. That is the whole reason the uncapped-billing signal exists — the
/// admission gate cannot see what only a spawned turn's init reports.
fn uncapped_harness(adapter: impl Fn() -> Box<dyn Adapter> + Send + 'static) -> (Harness, AgentId) {
    let config = Config {
        floor_usd_per_day: None,
        ..Harness::config()
    };
    let mut harness = Harness::with_config(
        config,
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        adapter,
    );
    let agent = hire_with(
        &mut harness,
        AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
    );
    set_goal_with(
        &mut harness,
        agent,
        GoalBudget {
            usd: None,
            tokens: None,
        },
    );
    (harness, agent)
}

/// `agent.hire` with an explicit budget, run to quiescence.
fn hire_with(harness: &mut Harness, budget: AgentBudget) -> AgentId {
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: std::path::PathBuf::from("/repo"),
        workspace: None,
        budget,
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
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

/// `goal.set` with an explicit budget, run to quiescence. This is what starts the loop.
fn set_goal_with(harness: &mut Harness, agent: AgentId, budget: GoalBudget) {
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "make the tests pass".into(),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["npm".into(), "test".into()],
            timeout_s: 600,
        },
        budget,
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
}

fn report_credential(harness: &mut Harness, agent: AgentId, source: Option<Credential>) {
    let turn = harness.open_turn(agent).expect("a turn is running");
    harness.drive(harness.session_observation(
        turn,
        SessionObservation {
            credential: source,
            ..SessionObservation::default()
        },
    ));
}

fn api_key() -> Credential {
    Credential::ApiKey("ANTHROPIC_API_KEY".into())
}

/// The signals of every `BreakerBeat` the machine asked to have written.
fn beat_signals(harness: &Harness) -> Vec<Vec<BreakerSignal>> {
    beats(harness)
        .into_iter()
        .map(|(_, _, signals)| signals)
        .collect()
}

/// **The controls run before the subject.**
///
/// A control placed after the subject never executes when the subject fails, so a mutation run
/// over this file would report nothing about it. Both arms say the same thing from opposite
/// sides: the predicate is about the *value* the vendor reported, not about the field being
/// there.
///
/// **Vacuity:** both arms drive a tick before asserting, and `an_uncapped_billing_source_beats_once_on_the_next_tick` shows the same fixture
/// reaching a beat — so "no beat" here is a fact about the value and not about a harness that
/// beats for nobody.
#[test]
fn a_reported_none_or_an_absent_source_raises_nothing() {
    for source in [Some(Credential::NoApiKey), None] {
        let (mut harness, agent) =
            uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
        report_credential(&mut harness, agent, source.clone());
        harness.drive(Input::Tick);
        assert!(
            beats(&harness).is_empty(),
            "an init reporting {source:?} must raise nothing, but the machine journaled {:?}",
            beat_signals(&harness)
        );
        assert_eq!(rung(&harness, agent), BreakerRung::Healthy);
    }
}

/// **A ceiling in any scope suppresses, and zero is a ceiling.**
///
/// Four `Option`s in three scopes make the conjunction, and each is asserted on its own: the
/// blast radius of deleting one read is one scope, so one mutant standing for four would prove
/// nothing about the other three. Zero is **not** here — it has its own arm below, for a reason
/// worth reading before adding a row to this loop.
///
/// **Vacuity, and it is enforced rather than asserted in prose.** Each row demands an open turn
/// before it feeds an init, because a ceiling that refuses at gate 5 leaves no turn, no init and
/// no latch — and a row like that would report "suppressed" whatever the predicate said. The
/// `unwrap_or_else` is what turns that into a failure instead of a false pass. The same fixture
/// with every ceiling absent is
/// `an_uncapped_billing_source_beats_once_on_the_next_tick`, which beats, so a suppressed row is the ceiling's doing.
#[test]
fn any_ceiling_in_any_scope_suppresses_the_signal() {
    for (what, config, agent_budget, goal_budget) in one_ceiling_per_scope() {
        let mut harness = Harness::with_config(
            config,
            FakeClock::ticking(1_787_394_600_000, 1),
            FakeHost::wall_clock_anchored(),
            || Box::new(ClaudeAdapter::new(ClaudeConfig::default())),
        );
        let agent = hire_with(&mut harness, agent_budget);
        set_goal_with(&mut harness, agent, goal_budget);
        let turn = harness.open_turn(agent).unwrap_or_else(|| {
            panic!("{what}: no turn is running, so this row would pass vacuously")
        });
        harness.drive(harness.session_observation(
            turn,
            SessionObservation {
                credential: Some(api_key()),
                ..SessionObservation::default()
            },
        ));
        harness.drive(Input::Tick);
        assert!(
            beats(&harness).is_empty(),
            "{what} must suppress the signal, but the machine journaled {:?}",
            beat_signals(&harness)
        );
    }
}

/// One set ceiling per scope and unit, each row absent everywhere else.
///
/// Four `Option`s live in three scopes, so a row per scope is what makes a deleted read fail by
/// name rather than being masked by a sibling that is still set.
fn one_ceiling_per_scope() -> Vec<(&'static str, Config, AgentBudget, GoalBudget)> {
    let floor = Config {
        floor_usd_per_day: Some(Usd::from_dollars(100)),
        ..Harness::config()
    };
    vec![
        (
            "a floor ceiling",
            floor,
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            GoalBudget {
                usd: None,
                tokens: None,
            },
        ),
        (
            "an agent dollar ceiling",
            Config {
                floor_usd_per_day: None,
                ..Harness::config()
            },
            AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            GoalBudget {
                usd: None,
                tokens: None,
            },
        ),
        (
            "an agent token ceiling",
            Config {
                floor_usd_per_day: None,
                ..Harness::config()
            },
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: Some(Tokens::new(1_000)),
            },
            GoalBudget {
                usd: None,
                tokens: None,
            },
        ),
        (
            "a goal dollar ceiling",
            Config {
                floor_usd_per_day: None,
                ..Harness::config()
            },
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
        ),
        (
            "a goal token ceiling",
            Config {
                floor_usd_per_day: None,
                ..Harness::config()
            },
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            GoalBudget {
                usd: None,
                tokens: Some(Tokens::new(1_000)),
            },
        ),
    ]
}

/// **Zero is a ceiling, measured where a zero ceiling can actually be observed.**
///
/// A ceiling has three states per unit — absent, zero and set — and zero is
/// **set**: it means *spend nothing*, not *spend anything*. The mutant is
/// `is_some_and(|c| c > Usd::ZERO)` in place of `is_some()`, which would treat the hardest cap in
/// the system as no cap at all.
///
/// **Why this arm is not a row of the loop above, which is the part worth keeping.** The obvious row
/// says *"the same with `usd_per_day: Some(Usd::ZERO)` → no `Persist`"*. Written that way here
/// it passes **vacuously**: a zero ceiling refuses at gate 5 before the first turn, so no turn
/// runs, no init arrives, nothing is latched, and the row reports "suppressed" with the `> 0`
/// mutant in place. Measured — the loop above fails that row with *"no turn is running, so this
/// row would pass vacuously"*.
///
/// So the zero ceiling has to arrive **after** the latch does. The agent starts ceiling-less, an
/// init latches a billing source and one beat proves it is raising — that assertion is this
/// arm's non-vacuity control — and only then does a goal ceiling of zero arrive. Under the
/// mutant the ladder keeps climbing; correctly, the condition clears and it descends.
#[test]
fn a_zero_ceiling_is_a_ceiling() {
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut harness, agent, Some(api_key()));
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Steer,
        "control: with no ceiling anywhere the condition is raising, so what follows is the \
         zero ceiling's doing and not an inert fixture"
    );

    set_goal_with(
        &mut harness,
        agent,
        GoalBudget {
            usd: Some(Usd::ZERO),
            tokens: None,
        },
    );
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Healthy,
        "a goal ceiling of zero is a ceiling, so the predicate is false and the ladder descends"
    );
}

/// An absent field raises nothing; this does not say what an absent field does to a
/// value already latched, and the two are different questions. This arm answers the second one in
/// the direction stated for the whole mechanism — it fails toward not raising, which costs
/// protection rather than issuing a false stop — so a vendor that stops reporting is treated as
/// nothing observed rather than as the last thing it said, for ever.
///
/// Without this arm, `None => {}` in place of `None => { remove }` is **green**: measured, no
/// other arm in this file distinguishes them, because every other absent-field arm starts from an
/// empty latch where clearing and not clearing look the same. The first tick here is what makes
/// the latch non-empty, and it is the control.
#[test]
fn an_absent_source_clears_a_standing_latch() {
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut harness, agent, Some(api_key()));
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Steer,
        "control: the latch is established and raising, so what follows is the absent field's \
         doing"
    );

    report_credential(&mut harness, agent, None);
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Healthy,
        "an init reporting nothing clears the latch, so the next beat is quiet and descends"
    );
}

/// **Zero is a ceiling in the agent scope, which no live command sequence can reach.**
///
/// The goal arm above cannot cover this read, and the difference is structural rather than an
/// oversight: an agent's ceiling is set once, at `agent.hire`, and **no record or command changes
/// it afterwards** — `AgentHired` is the only record in the workspace carrying an `AgentBudget`.
/// So a zero agent ceiling refuses at gate 5 before the first turn, no init ever arrives, and
/// nothing is latched to suppress. Measured: the mutant
/// `usd_per_day.is_some_and(|c| c > Usd::ZERO)` stays **green** against the goal arm alone.
///
/// A journal is the one thing that can hold the state a live sequence cannot build, so the world
/// is replayed instead of driven — the technique
/// [`assert_empty_and_inherited_inits_do_not_leak`] already uses for its inherit-mode agent. That
/// is not a contrivance: `World` is the fold of fsynced records, the machine must be correct over
/// any journal it is handed, and a turn that outlived a hire is exactly what a replay can hold.
///
/// **Non-vacuity is the whole risk here, so it is asserted twice.** A replayed fixture that
/// failed to produce a resolvable turn would suppress for the wrong reason and pass. So the arm
/// first runs the identical replay with `usd_per_day: None` and requires it to **raise** — if the
/// latch cannot be established this control fails — and only then runs the zero row.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the paired absent and zero controls retain their complete historical physical receipts"
)]
fn a_zero_agent_ceiling_is_a_ceiling() {
    for (budget, expected) in [
        (
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            BreakerRung::Steer,
        ),
        (
            AgentBudget {
                usd_per_day: Some(Usd::ZERO),
                tokens_per_day: None,
            },
            BreakerRung::Healthy,
        ),
    ] {
        let config = Config {
            floor_usd_per_day: None,
            ..Harness::config()
        };
        let mut harness = Harness::with_config(
            config,
            FakeClock::ticking(1_787_394_600_000, 1),
            FakeHost::wall_clock_anchored(),
            || Box::new(ClaudeAdapter::new(ClaudeConfig::default())),
        );
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
        let goal_id = dudes_types::GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("a goal id");
        let turn = dudes_types::TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("a turn id");
        let at = Timestamp::from_unix_millis(1_787_394_600_000);

        replay(
            &mut harness,
            1,
            at,
            Record::AgentHired {
                character: None,
                accent: None,
                id: agent,
                name: "Hector".to_owned(),
                role: "infra".to_owned(),
                department: "infra".to_owned(),
                provider: Provider::Claude,
                cwd: "/repo".into(),
                budget,
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
        );
        replay(
            &mut harness,
            2,
            at,
            Record::GoalSet {
                agent,
                goal: dudes_proto::Goal {
                    completion_check: None,
                    id: goal_id,
                    text: "make the tests pass".to_owned(),
                    predicate: dudes_proto::Predicate::Command {
                        argv: vec!["npm".to_owned(), "test".to_owned()],
                        timeout_s: 600,
                    },
                    budget: GoalBudget {
                        usd: None,
                        tokens: None,
                    },
                    hop_cap: 8,
                    baseline: None,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        );
        replay(
            &mut harness,
            3,
            at,
            Record::TurnIssued {
                agent,
                turn,
                goal: goal_id,
                task: "make the tests pass".to_owned(),
                session: None,
                mode: dudes_types::Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn(1),
                answers: Vec::new(),
            },
        );

        let key = harness.host.key_for(1000);
        replay(
            &mut harness,
            4,
            at,
            Record::TurnSpawned {
                agent,
                turn,
                pid: key.pid,
                start_time: key.start_time,
                boot: key.boot,
                session: None,
                reissued: false,
                mcp_grant: None,
            },
        );

        harness.drive(harness.session_observation(
            turn,
            SessionObservation {
                credential: Some(api_key()),
                ..SessionObservation::default()
            },
        ));
        harness.drive(Input::Tick);
        assert_eq!(
            rung(&harness, agent),
            expected,
            "agent budget {budget:?}: an absent ceiling must raise and a zero one must not"
        );
    }
}

/// **The first arm.**
///
/// A reported billing source on a ceiling-less agent, then **one tick**, is exactly one
/// `BreakerBeat` moving `Healthy → Steer` and naming the signal. Kills deleting the arm, and
/// kills a stream path that beats on its own input: a `beat` from `observe_billing` would put the
/// record on the disk *before* the tick, which is forbidden because an out-of-band beat
/// is also where a descent happens.
///
/// **Vacuity:** `beats` is asserted **empty before** the tick, so the arm cannot pass on a record
/// the fixture produced for itself.
#[test]
fn an_uncapped_billing_source_beats_once_on_the_next_tick() {
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut harness, agent, Some(api_key()));
    assert!(
        beats(&harness).is_empty(),
        "the session observation must not beat: the signal waits for the tick"
    );

    harness.drive(Input::Tick);
    assert_eq!(
        beats(&harness),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            vec![BreakerSignal::UncappedBillingSource]
        )],
        "one init and one tick is one beat naming the signal"
    );
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
}

/// **Two observations of one init produce one beat carrying both.**
///
/// This is the arm that pins the *ordering*, and it fails in both directions. Put the
/// billing block **after** `observe_isolation` and its `beat` has already drained the set, so the
/// billing signal waits for the next tick: two records for one init. Leave the early return at
/// the top of `observe_isolation` in front of the billing block and the billing signal vanishes
/// on every turn that is not also a leak.
///
/// `signals` is asserted as the **sorted pair**, which is `BreakerSignal`'s declaration order
/// through the `BTreeSet` — the reason new signals are appended, never inserted.
///
/// **Vacuity:** the count and the contents are both asserted, so neither a lost signal nor a
/// doubled beat can pass; and `ConfigDirAdapter` is the fixture the neighbouring
/// `assert_plugins_leak_for_flags_and_config_dir` already proves does leak.
#[test]
fn one_init_with_both_observations_produces_one_beat() {
    let (mut harness, agent) = uncapped_harness(|| {
        Box::new(ConfigDirAdapter(
            ClaudeAdapter::new(ClaudeConfig::default()),
        ))
    });
    let turn = harness.open_turn(agent).expect("a turn is running");
    harness.drive(harness.session_observation(
        turn,
        SessionObservation {
            servers: vec!["repository-tools".into()],
            credential: Some(api_key()),
            ..SessionObservation::default()
        },
    ));

    assert_eq!(
        beats(&harness),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            vec![
                BreakerSignal::IsolationLeak,
                BreakerSignal::UncappedBillingSource
            ]
        )],
        "one init carrying both observations is one beat carrying both signals, sorted"
    );
}

/// **The cadence arm, and the one without which this mechanism stops nothing.**
///
/// Every other arm asserts at most one beat, and gate 5 reads `Constrain`. A signal raised once
/// per init against a ladder that descends on every quiet 30-second beat oscillates
/// `Steer → Healthy → Steer` for ever and never arrives, so the mechanism would warn and stop
/// nothing. Level-triggering is what makes the rung stand: the tick re-reads the latch and
/// re-raises while the condition holds.
///
/// Kills edge-triggering, a latch cleared by `beat`'s drain rather than by the condition, and a
/// tick that re-raises unconditionally.
///
/// **The control is the load-bearing half.** Without it this arm passes on a tick that raises for
/// everybody: the identical fixture reporting no API key must stay `Healthy` across the same three
/// ticks and leave gate 5 alone. It runs **first**.
#[test]
fn a_held_uncapped_billing_source_climbs_to_stop() {
    // Control first: the same three ticks over a fixture whose only difference is the value.
    let (mut quiet, quiet_agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut quiet, quiet_agent, Some(Credential::NoApiKey));
    for _ in 0..3 {
        quiet.drive(Input::Tick);
    }
    assert!(
        beats(&quiet).is_empty(),
        "control: a reported `none` must not climb, but the machine journaled {:?}",
        beat_signals(&quiet)
    );
    assert_eq!(rung(&quiet, quiet_agent), BreakerRung::Healthy);

    // Subject: one init, then three ticks and no further init.
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut harness, agent, Some(api_key()));
    for _ in 0..3 {
        harness.drive(Input::Tick);
    }
    let climbed: Vec<BreakerRung> = beats(&harness)
        .into_iter()
        .map(|(_, rung_after, _)| rung_after)
        .collect();
    assert_eq!(
        climbed[..3],
        [
            BreakerRung::Steer,
            BreakerRung::Constrain,
            BreakerRung::Stop
        ],
        "a held condition climbs one rung per beat: {climbed:?}"
    );
    assert!(
        rung(&harness, agent) >= BreakerRung::Constrain,
        "the rung gate 5 reads must stand at or above Constrain"
    );
}

/// **The second half: the rung the ladder reached is the one gate 5 refuses on.**
///
/// Separated from the climb so a failure says which half broke. The agent's turn ends honestly
/// and its predicate fails, so gates 1–4 pass the decision down to gate 5, which reads the rung
/// the billing signal put there.
#[test]
fn a_held_uncapped_billing_source_exhausts_at_gate_five() {
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    let turn = harness.open_turn(agent).expect("a turn is running");
    let goal = *harness
        .supervisor
        .world()
        .goals
        .keys()
        .next_back()
        .expect("the goal was journaled");
    report_credential(&mut harness, agent, Some(api_key()));
    for _ in 0..2 {
        harness.drive(Input::Tick);
    }
    assert_eq!(rung(&harness, agent), BreakerRung::Constrain);

    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });

    let (decision, note) = last_decision(&harness);
    assert_eq!(
        decision,
        LoopDecision::Exhausted {
            reason: ExhaustedReason::Breaker
        },
        "gate 5 refuses the next turn on the rung this signal raised; note: {note}"
    );
}

/// This follows from level-triggering rather than a separate mechanism,
/// and it is what makes the volume bounded in the ordinary case. It is also the arm that proves
/// the latch is a *level* and not a one-shot: a design that latched only the first observation
/// would keep climbing here.
#[test]
fn a_later_none_clears_the_condition_and_the_ladder_descends() {
    let (mut harness, agent) =
        uncapped_harness(|| Box::new(ClaudeAdapter::new(ClaudeConfig::default())));
    report_credential(&mut harness, agent, Some(api_key()));
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);

    report_credential(&mut harness, agent, Some(Credential::NoApiKey));
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Healthy,
        "a later `none` clears the latch, so the next beat is quiet and descends"
    );
}

/// Deliver one already-normalised hook edge through the supervisor's production input seam.
fn hook(harness: &mut Harness, agent: AgentId, kind: EventKind) {
    harness.drive(Input::Hook {
        hook: HookId(1),
        event: Event {
            agent,
            at: Timestamp::UNIX_EPOCH,
            provenance: Provenance::Observed,
            kind,
            raw: None,
        },
    });
}

/// The event kind distinguishes compaction entry from exit, and a test
/// asserts the supervisor receives a true compaction state derived from it.
///
/// Outside a compaction window, PTY silence counts. A start edge makes the same signal
/// non-counting, while another signal continues to climb the ladder inside that window.
#[test]
fn compact_start_drives_a_true_compacting_beat() {
    let clock = FakeClock::frozen(1_000);
    let mut harness = Harness::with(clock, FakeHost::wall_clock_anchored());
    let agent = harness.hire();

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);

    hook(&mut harness, agent, EventKind::CompactStart);
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(
        beats(&harness).last(),
        Some(&(
            BreakerRung::Healthy,
            BreakerRung::Healthy,
            vec![BreakerSignal::PtySilence]
        )),
        "the exempt signal is journalled even when it does not raise the rung"
    );

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::HookTimeout,
    });
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Steer,
        "compaction exempts PtySilence, not every signal"
    );
}

/// **First half of the expiry rule:** the exemption expires without an end event,
/// and an end with nothing in flight changes nothing.
///
/// Three arms, in order. The lone end is the no-op — it cannot open a window.
/// The middle arm is the subject: a start with no end climbs one full grace later. The
/// trailing arm is the control — the same sequence *with* an end inside the grace,
/// climbing one trailing grace later — which is what makes the pair a test about the deadline and
/// not about a ladder that was stuck.
#[test]
fn compact_end_shortens_only_a_live_window() {
    let lone_end_clock = FakeClock::frozen(0);
    let mut lone_end = Harness::with(lone_end_clock.clone(), FakeHost::wall_clock_anchored());
    let lone_end_agent = lone_end.hire();
    hook(&mut lone_end, lone_end_agent, EventKind::CompactEnd);
    lone_end.drive(Input::Signal {
        agent: lone_end_agent,
        signal: RuntimeSignal::PtySilence,
    });
    lone_end.drive(Input::Tick);
    assert_eq!(rung(&lone_end, lone_end_agent), BreakerRung::Steer);

    let full_clock = FakeClock::frozen(0);
    let mut full = Harness::with(full_clock.clone(), FakeHost::wall_clock_anchored());
    let full_agent = full.hire();
    hook(&mut full, full_agent, EventKind::CompactStart);
    full_clock.advance(COMPACTION_GRACE_MS - 1);
    full.drive(Input::Signal {
        agent: full_agent,
        signal: RuntimeSignal::PtySilence,
    });
    full.drive(Input::Tick);
    assert_eq!(rung(&full, full_agent), BreakerRung::Healthy);
    full_clock.advance(1);
    full.drive(Input::Signal {
        agent: full_agent,
        signal: RuntimeSignal::PtySilence,
    });
    full.drive(Input::Tick);
    assert_eq!(rung(&full, full_agent), BreakerRung::Steer);

    let trailing_clock = FakeClock::frozen(0);
    let mut trailing = Harness::with(trailing_clock.clone(), FakeHost::wall_clock_anchored());
    let trailing_agent = trailing.hire();
    hook(&mut trailing, trailing_agent, EventKind::CompactStart);
    hook(&mut trailing, trailing_agent, EventKind::CompactEnd);
    trailing_clock.advance(COMPACTION_TRAILING_GRACE_MS - 1);
    trailing.drive(Input::Signal {
        agent: trailing_agent,
        signal: RuntimeSignal::PtySilence,
    });
    trailing.drive(Input::Tick);
    assert_eq!(rung(&trailing, trailing_agent), BreakerRung::Healthy);
    trailing_clock.advance(1);
    trailing.drive(Input::Signal {
        agent: trailing_agent,
        signal: RuntimeSignal::PtySilence,
    });
    trailing.drive(Input::Tick);
    assert_eq!(rung(&trailing, trailing_agent), BreakerRung::Steer);
}

/// **compaction exemption:** an end shortens a live window; it may never lengthen one.
///
/// The twin of the test above, which pins the shortening. A `CompactEnd` arriving with less than
/// one trailing grace left computes a trailing deadline *beyond* the original, and the `min` in
/// `Compaction::end` is the whole of what discards it. Without that clamp a late end buys the
/// caller up to a further `COMPACTION_TRAILING_GRACE_MS` of suppressed beats that no `CompactStart`
/// could — the harm the suppression ceiling exists to bound, reached through the direction that is
/// calls a no-op, and against the name of the test above.
#[test]
fn compact_end_never_lengthens_a_window() {
    let clock = FakeClock::frozen(0);
    let mut harness = Harness::with(clock.clone(), FakeHost::wall_clock_anchored());
    let agent = harness.hire();
    hook(&mut harness, agent, EventKind::CompactStart);

    let elapsed = COMPACTION_GRACE_MS - COMPACTION_TRAILING_GRACE_MS + 1;
    clock.advance(elapsed);
    hook(&mut harness, agent, EventKind::CompactEnd);

    clock.advance(COMPACTION_GRACE_MS - elapsed);
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Steer,
        "the original deadline still bounds the exemption after a late end"
    );
}

/// **A repeated start does not extend a window:** a `CompactStart` arriving while a window is in
/// flight does not extend it.
///
/// A repeated start keeps the first deadline. Replacing that edge with an end is the positive
/// control: it moves the deadline earlier through the same hook path, so the test
/// can tell *the second start did nothing* from *no event on this path does anything*.
#[test]
fn compact_start_does_not_extend_a_live_window() {
    let start_clock = FakeClock::frozen(0);
    let mut repeated_start = Harness::with(start_clock.clone(), FakeHost::wall_clock_anchored());
    let start_agent = repeated_start.hire();
    hook(&mut repeated_start, start_agent, EventKind::CompactStart);
    start_clock.advance(COMPACTION_TRAILING_GRACE_MS);
    hook(&mut repeated_start, start_agent, EventKind::CompactStart);
    start_clock.advance(COMPACTION_TRAILING_GRACE_MS);
    repeated_start.drive(Input::Signal {
        agent: start_agent,
        signal: RuntimeSignal::PtySilence,
    });
    repeated_start.drive(Input::Tick);
    assert_eq!(rung(&repeated_start, start_agent), BreakerRung::Healthy);
    start_clock.advance(COMPACTION_GRACE_MS - 2 * COMPACTION_TRAILING_GRACE_MS);
    repeated_start.drive(Input::Signal {
        agent: start_agent,
        signal: RuntimeSignal::PtySilence,
    });
    repeated_start.drive(Input::Tick);
    assert_eq!(rung(&repeated_start, start_agent), BreakerRung::Steer);

    let end_clock = FakeClock::frozen(0);
    let mut ended = Harness::with(end_clock.clone(), FakeHost::wall_clock_anchored());
    let end_agent = ended.hire();
    hook(&mut ended, end_agent, EventKind::CompactStart);
    end_clock.advance(COMPACTION_TRAILING_GRACE_MS);
    hook(&mut ended, end_agent, EventKind::CompactEnd);
    end_clock.advance(COMPACTION_TRAILING_GRACE_MS);
    ended.drive(Input::Signal {
        agent: end_agent,
        signal: RuntimeSignal::PtySilence,
    });
    ended.drive(Input::Tick);
    assert_eq!(
        rung(&ended, end_agent),
        BreakerRung::Steer,
        "the end edge proves this hook path can shorten a live deadline"
    );
}

/// **compaction exemption:** the exemption lives in memory, is not journalled and is not replayed —
/// and losing it fails toward *counting* signals, which is the safe direction.
///
/// Compaction state is deliberately run-local: replay restores the rung, not a live exemption.
#[test]
fn restart_forgets_compaction_toward_counting() {
    let clock = FakeClock::frozen(0);
    let mut harness = Harness::with(clock, FakeHost::wall_clock_anchored());
    let agent = harness.hire();
    hook(&mut harness, agent, EventKind::CompactStart);
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);

    harness.restart();
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
}

/// **The ceiling binds across window boundaries, and the assertion is
/// about the rung.
///
/// A fresh start after every expired window cannot refill the suppression budget. **This is the
/// only test in the tree that separates the accepted reset — keyed on the agent's own
/// evidence of life — from a reset keyed on windows or starts.** The ceiling-plus-one
/// fixture is not ceremony, it is the control. A reset a window or a fresh
/// `CompactStart` could trigger yields one cancelled climb per cycle and never leaves the bottom
/// two rungs, which still satisfies a check asserting only a *deadline*. Reading the rung, and
/// reading it again with the ceiling raised by one, is what says the **ceiling** fired and not a
/// grace that happened to expire. Delete either arm and the rejected reset passes here.
#[test]
fn compaction_ceiling_reaches_the_cap_across_windows() {
    fn first_cap_beat(ceiling: u8) -> (usize, BreakerRung) {
        let clock = FakeClock::frozen(0);
        let mut harness = Harness::with(clock.clone(), FakeHost::wall_clock_anchored());
        harness.set_compaction_max_suppressed_beats(ceiling);
        let agent = harness.hire();
        let mut first_cap = None;

        for beat in 1..=usize::from(ceiling) + 4 {
            hook(&mut harness, agent, EventKind::CompactStart);
            harness.drive(Input::Signal {
                agent,
                signal: RuntimeSignal::PtySilence,
            });
            harness.drive(Input::Tick);
            if rung(&harness, agent) == Harness::config().breaker.cap() && first_cap.is_none() {
                first_cap = Some(beat);
            }
            clock.advance(COMPACTION_GRACE_MS);
        }

        (
            first_cap.expect("the bounded exemption reaches the configured cap"),
            rung(&harness, agent),
        )
    }

    let (default_cap_beat, default_final_rung) = first_cap_beat(COMPACTION_MAX_SUPPRESSED_BEATS);
    let (raised_cap_beat, raised_final_rung) = first_cap_beat(COMPACTION_MAX_SUPPRESSED_BEATS + 1);
    assert_eq!(
        raised_cap_beat,
        default_cap_beat + 1,
        "raising only the ceiling by one delays the cap by exactly one beat"
    );
    assert_eq!(default_final_rung, Harness::config().breaker.cap());
    assert_eq!(raised_final_rung, Harness::config().breaker.cap());
}

/// **compaction exemption:** the count resets on the first beat whose collected signals contain no
/// `COMPACTION_EXEMPT` signal — the agent's evidence of life — and on nothing else.
///
/// A beat with no exempt signal is the one event that refills the suppression budget. A window
/// closing, a fresh `CompactStart` and the budget being spent reset nothing, which is what stops a
/// silent agent refilling it however many windows open and close.
#[test]
fn a_non_exempt_beat_refills_the_compaction_ceiling() {
    let clock = FakeClock::frozen(0);
    let mut harness = Harness::with(clock, FakeHost::wall_clock_anchored());
    harness.set_compaction_max_suppressed_beats(1);
    let agent = harness.hire();
    hook(&mut harness, agent, EventKind::CompactStart);

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::HookTimeout,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::PtySilence,
    });
    harness.drive(Input::Tick);
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Healthy,
        "the live window can suppress again only after a beat with no exempt signal"
    );
}

/// **compaction exemption:** the exemption's state is run-local, and only an agent with a ladder
/// allocates in it.
///
/// `on_hook`'s `has_breaker` filter, which is deliberately *not* the filter `on_signal` refuses:
/// `on_tick` clears `Supervisor::signals` every beat, so an entry there for an agent with no
/// ladder costs one beat. Retirement can prune `Supervisor::compaction`, but after an invented
/// `AgentId` allocates, no future retirement fold is guaranteed; cleanup depends on a separate
/// later operator action. Unfiltered, one `hook.event` call per invented `AgentId` grows the map
/// without bound.
///
/// The live agent is the positive control: without it, an assertion that the map did not grow
/// would hold just as well on a machine where the map never grew for anybody.
#[test]
fn a_compact_start_allocates_only_for_an_agent_with_a_ladder() {
    let mut harness = Harness::new();
    let live = harness.hire();
    hook(&mut harness, live, EventKind::CompactStart);
    assert_eq!(
        harness.compaction_entries(),
        1,
        "an agent with a ladder allocates, so a refusal below is a refusal and not an inert map"
    );

    let never_hired = AgentId::from_parts(1_787_394_600_000, 7);
    hook(&mut harness, never_hired, EventKind::CompactStart);
    assert_eq!(
        harness.compaction_entries(),
        1,
        "an AgentId the daemon cannot resolve to an agent allocates nothing"
    );

    let retired = harness.hire();
    harness.drive(Input::Command(Command::Retire {
        agent: retired,
        reason: "done for the day".into(),
        browser_retire: None,
    }));
    hook(&mut harness, retired, EventKind::CompactStart);
    assert_eq!(
        harness.compaction_entries(),
        1,
        "a retired agent has no ladder, so its window has nothing to exempt"
    );
}

/// A compaction entry is run-local state for a live agent, so durable retirement releases it.
///
/// The live count is the positive control: it proves the hook allocated before retirement, while
/// the pre-hire count makes the final assertion independent of any entries the harness may gain.
#[test]
fn retiring_an_agent_prunes_its_compaction_entry() {
    let mut harness = Harness::new();
    let pre_hire = harness.compaction_entries();
    let agent = harness.hire();

    hook(&mut harness, agent, EventKind::CompactStart);
    assert_eq!(
        harness.compaction_entries(),
        pre_hire + 1,
        "a live agent's compact start allocates its run-local entry"
    );

    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "done for the day".into(),
        browser_retire: None,
    }));
    assert_eq!(
        harness.compaction_entries(),
        pre_hire,
        "durable retirement releases the agent's run-local compaction entry"
    );
}

/// A `Tick` with a signal raises the rung by one and journals `BreakerBeat`.
///
/// One rung, not two, for one beat — and the record carries the rung on both
/// sides of the move plus the signal that caused it, so `dudes why` can say why the loop tightened
/// without re-deriving it.
#[test]
fn a_signal_raises_the_breaker_one_rung_on_the_next_tick() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);

    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::HookTimeout,
    });
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Healthy,
        "the signal alone moves nothing; the beat does"
    );

    harness.drive(Input::Tick);

    assert_eq!(
        beats(&harness),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            vec![BreakerSignal::HookTimeout]
        )]
    );
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
}

/// A healthy `Tick` lowers the rung.
///
/// Recovery is journaled as deliberately as escalation: one rung down per quiet beat, each one a
/// record, so the fold a replay rebuilds descends the same ladder the live run did. A beat that
/// neither moves the rung nor saw a signal writes nothing — otherwise an idle floor would append
/// two records a minute per agent forever.
#[test]
fn a_quiet_tick_lowers_the_breaker_one_rung_and_a_healthy_one_writes_nothing() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);

    for _ in 0..2 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::PtySilence,
        });
        harness.drive(Input::Tick);
    }
    assert_eq!(rung(&harness, agent), BreakerRung::Constrain);

    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);

    let descents = &beats(&harness)[2..];
    assert_eq!(
        descents,
        [
            (BreakerRung::Constrain, BreakerRung::Steer, vec![]),
            (BreakerRung::Steer, BreakerRung::Healthy, vec![]),
        ]
    );

    // Already `Healthy` and nothing was seen: the beat has nothing to say.
    let before = beats(&harness).len();
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);
    assert_eq!(
        beats(&harness).len(),
        before,
        "a beat that neither moves the rung nor saw a signal writes no record"
    );
}

/// Reaching `Stop` emits a `Kill` for a running turn.
///
/// The breaker joins `Halt` and `AgentRetired` as a standing cause, so the
/// kill is the same one function, reads the rung back out of the *fsynced* fold, and stamps the
/// end `Killed{BreakerStop}` rather than whatever the adapter read off a signalled process.
#[test]
fn the_breaker_reaching_stop_kills_the_running_turn() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    for expected in [
        BreakerRung::Steer,
        BreakerRung::Constrain,
        BreakerRung::Stop,
    ] {
        assert!(
            harness
                .trace
                .iter()
                .all(|seen| !matches!(seen, Seen::Kill { .. })),
            "nothing below Stop kills a running turn"
        );
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
        assert_eq!(rung(&harness, agent), expected);
    }

    let killed: Vec<&Seen> = harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Kill { .. }))
        .collect();
    assert!(
        matches!(killed.as_slice(), [Seen::Kill { agent: a, turn: t }] if *a == agent && *t == turn),
        "exactly one Kill, for this agent's running turn: {killed:?}"
    );

    // And the end it produces names the breaker, not the signal the process died of.
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: None,
            signal: Some(15),
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Failed {
            reason: "killed".into(),
        },
        last_message: None,
    });
    assert!(
        harness.records().any(|record| matches!(
            record,
            Record::TurnEnded {
                outcome: Some(TurnOutcome::Killed {
                    by: KillReason::BreakerStop
                }),
                ..
            }
        )),
        "the ledger's reason wins over the adapter's"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Killed
    );
}

/// Gate 5 then refuses admission.
///
/// The whole point of the rung. Before it existed nothing moved it, so `ExhaustedReason::Breaker`
/// was unreachable however the loop behaved — a gate that cannot fire. The note is asserted too,
/// because "refused" and "refused *for this reason*" are what `dudes why` has to tell apart.
#[test]
fn gate_5_refuses_admission_once_the_breaker_stands_at_constrain() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    for _ in 0..2 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::UnackedMail,
        });
        harness.drive(Input::Tick);
    }
    assert_eq!(rung(&harness, agent), BreakerRung::Constrain);

    // End the turn honestly and let the loop decide again: predicate fails, so gate 4 passes the
    // decision down to gate 5.
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });

    let (decision, note) = last_decision(&harness);
    assert_eq!(
        decision,
        LoopDecision::Exhausted {
            reason: ExhaustedReason::Breaker
        }
    );
    assert_eq!(note, "breaker at Constrain");
    assert_eq!(
        harness.open_turn(agent),
        None,
        "and no second turn was issued"
    );
}

/// Signals are collected per beat, not per event.
///
/// The clause most likely to pass by accident, so it is asserted three ways at once: four signals
/// inside one window produce **one** record, **one** rung of movement, and a signal list that is
/// the *set* of kinds seen — the repeat of `HookTimeout` is not a second entry, because
/// multiplicity carries no decision and an unbounded list in a journal record is a cost with no
/// reader.
#[test]
fn breaker_signals_are_collected_per_beat_not_per_event() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);

    for signal in [
        RuntimeSignal::HookTimeout,
        RuntimeSignal::UnackedMail,
        RuntimeSignal::HookTimeout,
        RuntimeSignal::PtySilence,
    ] {
        harness.drive(Input::Signal { agent, signal });
    }
    assert!(
        beats(&harness).is_empty(),
        "four signals and no beat wrote nothing at all"
    );

    harness.drive(Input::Tick);

    assert_eq!(
        beats(&harness),
        [(
            BreakerRung::Healthy,
            BreakerRung::Steer,
            // `BreakerSignal`'s declaration order, which is what a `BTreeSet` sorts by.
            vec![
                BreakerSignal::PtySilence,
                BreakerSignal::HookTimeout,
                BreakerSignal::UnackedMail,
            ]
        )],
        "one beat, one rung, one entry per kind seen"
    );

    // And the window really did close: the next beat starts from an empty collection.
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Healthy);
}

/// A retired agent has no breaker: its ladder stops where it stood, and no further beat is written
/// for it — neither up on a signal nor down on a quiet tick.
///
/// The rung is raised *before* the retirement on purpose, and that setup is what gives the descent
/// half an assertion that can fail. A retired agent left at `Healthy` is silent whether or not the
/// tick filters it out — an unchanged rung with no signals writes nothing either way — so with the
/// setup raise gone and the post-retire signal gone, deleting `on_tick`'s `!row.retired` filter
/// leaves this test green. One standing at `Steer` has a descent to write, and its absence is the
/// assertion.
///
/// The test is over-determined, and deliberately so: the post-retire signal and tick catch that
/// same mutation independently, by the climb, so removing only the setup raise still fails. Keep
/// both — one covers a retired ladder going down, the other covers it going up.
#[test]
fn a_retired_agent_gets_no_breaker_beat() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::HookTimeout,
    });
    harness.drive(Input::Tick);
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
    let before = beats(&harness).len();

    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "done for the day".into(),
        browser_retire: None,
    }));
    harness.drive(Input::Tick);
    harness.drive(Input::Signal {
        agent,
        signal: RuntimeSignal::HookTimeout,
    });
    harness.drive(Input::Tick);

    assert_eq!(
        beats(&harness).len(),
        before,
        "no beat descends or climbs a retired agent's ladder"
    );
    assert_eq!(rung(&harness, agent), BreakerRung::Steer);
}

// ---- the runtime seam ------------------------------------------------------------------------

/// The journaled signals a runtime can produce. Every walk over the seam below goes through here
/// rather than writing the four names out, and the equation the seam's shape rests on is asserted
/// as the list is built.
///
/// **The equation, and what it is not.** The image of `RuntimeSignal::ALL` under `signal()` is
/// asserted equal to `BreakerSignal::ALL` minus `InferredTurnEnd`, `IsolationLeak` and
/// `UncappedBillingSource`, the three signals with no `RuntimeSignal` emitter. Neither side is a table this
/// file maintains — `BreakerSignal::ALL` is pinned to serde's own derive by `dudes-types`'
/// `breaker_signal_all_is_every_variant`, so it cannot be edited into agreement without editing
/// the enum. `emitted` is ascending because `Ord` on `BreakerSignal` is its declaration order and
/// that order is what the derivation test pins, so sorting the image is the whole comparison.
///
/// It is an **equality**, and nothing else in the suite asserts one. It is one-directional — no v1
/// emitter writes an `Inferred` turn end — and in the same breath it names a second path: a
/// signal the *supervisor* raises for gate 2 goes straight into `Supervisor::signals` without
/// crossing this seam. So a journaled signal with no runtime emitter is something the code permits and
/// this equation forbids. That is deliberate, because it fails **safe**: such a variant reddens
/// here rather than passing, and the subset form (`image ⊆ emitted`) would notice nothing at all.
/// `IsolationLeak` was the first variant added with no runtime emitter and
/// `UncappedBillingSource` the second, so the exception is now the three-name set
/// above — both are raised straight into `Supervisor::signals`, the path the
/// `RuntimeSignal` doc authorises. The equality remains exact rather than weakening to a subset.
///
/// **What this catches, exactly** (each row checked by mutation):
///
/// | a fifth `RuntimeSignal`… | here |
/// | --- | --- |
/// | mapped to a **new** `BreakerSignal`, absent from `RuntimeSignal::ALL` | red — `emitted` grew, the image did not |
/// | mapped to an **existing** `BreakerSignal`, in `RuntimeSignal::ALL` | red — the image repeats an entry |
/// | mapped to a **new** `BreakerSignal`, **both** `ALL`s updated | **green — both sides grow and cancel** |
/// | mapped to an **existing** `BreakerSignal`, absent from `RuntimeSignal::ALL` | **green — neither side moves** |
///
/// So the honest form of the claim is narrow: **a widening is caught here only when it does not
/// also grow `BreakerSignal::ALL`.** A widening that grows both is caught by the classification
/// arm in `the_breaker_seam_carries_nothing_raised_by_what_a_turn_cost`, and only when the new
/// signal is classified `Cost` — so **a conduct-classified widening with both `ALL`s updated is
/// caught by nothing in this workspace.** The same mutation classified `Cost` fails this test.
///
/// A hand-written literal such as `[HookTimeout, PtySilence, StopCap, UnackedMail]` would notice
/// every widening and state the rule wrongly; this states the rule and notices fewer. The trade
/// is taken knowingly, not by accident.
///
/// **Why `RuntimeSignal::ALL` is not simply anchored.** `signal()` being a total match does not
/// force the array to grow: a fifth variant is a compile error in `signal()`, but a `[Self; 4]`
/// left at four still compiles. Nor can this file catch that by matching — `RuntimeSignal` is
/// `#[non_exhaustive]` and this is a different crate, so any match here needs a `_` arm. It is
/// **not out of reach**: a `macro_rules!` that declares the variants and expands to both the enum
/// and its `ALL` closes it with no dependency, no derive macro and no build script, and keeps
/// `#[non_exhaustive]`. The array's own doc comment says it is unguarded.
fn reachable_signals() -> Vec<BreakerSignal> {
    let reachable: Vec<BreakerSignal> = RuntimeSignal::ALL
        .iter()
        .map(|signal| signal.signal())
        .collect();
    let mut sorted = reachable.clone();
    sorted.sort_unstable();
    let emitted: Vec<BreakerSignal> = BreakerSignal::ALL
        .into_iter()
        .filter(|signal| {
            !matches!(
                signal,
                BreakerSignal::InferredTurnEnd
                    | BreakerSignal::IsolationLeak
                    | BreakerSignal::UncappedBillingSource
                    | BreakerSignal::RapidFailure
            )
        })
        .collect();
    assert_eq!(
        sorted, emitted,
        "the runtime seam's image must be every journaled signal except InferredTurnEnd, \
         IsolationLeak and UncappedBillingSource, once each"
    );
    reachable
}

/// What raises a signal, in the one distinction that matters.
///
/// The twin of `dudes-policy`'s test-local `raised_by`, and re-stated rather than exported on
/// purpose: that one classifies the signals the *ladder* can be handed, this one the signals a
/// *runtime* can produce, and the seam exists because those two sets differ. **It is not another
/// hand-written list** — `BreakerSignal` is not `#[non_exhaustive]`, so this match is exhaustive
/// over the enum and a seventh variant stops the build here. A list can fall behind an enum in
/// silence; an exhaustive match cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaisedBy {
    /// Something the daemon watched happen — how a turn ended, silence, a missed deadline, a cap,
    /// a notification nobody acknowledged.
    Conduct,
    /// What a turn cost, or the price put on an end that reported no cost at all — the
    /// `unknown_ends × per_turn_floor` reserve. It must never reach the breaker.
    Cost,
}

/// Total over `BreakerSignal`: an eighth variant is a compile error here, which is the point —
/// whoever names a signal after what a turn cost has to come to this seam to make the tree build.
///
/// The per-variant reasons: `InferredTurnEnd` is raised by the **provenance** of an end and never
/// by its usage; `PtySilence` by quiet on a PTY; `HookTimeout` by a deadline the supervisor
/// missed; `IsolationLeak` by observed vendor init state; `StopCap` by a steer hitting the
/// vendor's consecutive-block cap; `UnackedMail` by five unacknowledged notifications;
/// `UncappedBillingSource` by a vendor-reported credential source beside four absent ceiling
/// `Option`s, which reads no `usage` and fires the same on a zero-cost turn. None
/// is a figure anybody priced. The or-pattern is not laziness: `clippy::match_same_arms` is on,
/// so seven arms saying one thing must be written as one.
const fn raised_by(signal: BreakerSignal) -> RaisedBy {
    match signal {
        BreakerSignal::InferredTurnEnd
        | BreakerSignal::PtySilence
        | BreakerSignal::HookTimeout
        | BreakerSignal::IsolationLeak
        | BreakerSignal::StopCap
        | BreakerSignal::UnackedMail
        | BreakerSignal::UncappedBillingSource
        | BreakerSignal::RapidFailure => RaisedBy::Conduct,
    }
}

/// Invariant 3 at the runtime seam: the breaker's *input* seam cannot carry `InferredTurnEnd`.
///
/// No v1 emitter writes an `Inferred` turn end, which is what makes gate 2's
/// `Hold` transient by construction. `decide()` still knows the variant — gate 2 must not know
/// which emitters exist (invariant 5) — but the runtime's way in is [`RuntimeSignal`], and its
/// mapping is total over four variants none of which is that one.
///
/// **What this does and does not notice.** It is about `InferredTurnEnd` specifically, not about
/// the seam's width: [`reachable_signals`] pins the image to `BreakerSignal::ALL` minus this
/// variant, `IsolationLeak` and `UncappedBillingSource`, so a runtime cannot hand *this* one in, and a widening is caught only in the two
/// cases that table names — never when the new signal grows `BreakerSignal::ALL` too. The seam
/// can widen without a design change, and does so green. Read [`reachable_signals`] for which widenings land where.
///
/// The two halves below do not carry the same weight, and the difference is worth stating.
/// The first is structural: [`reachable_signals`] pins the seam's image to `BreakerSignal::ALL`
/// minus this variant, `IsolationLeak` and `UncappedBillingSource`, so a runtime cannot hand it in. The second is
/// *behavioural* and v1-only —
/// `Record::BreakerBeat.signals` is a `Vec<BreakerSignal>`, so the variant is perfectly
/// representable in a journaled beat; what is asserted is that this machine puts none there today,
/// which stops holding the day the supervisor raises gate 2's own signal. Both
/// halves are kept: they protect different things.
#[test]
fn the_breaker_seam_cannot_carry_an_inferred_turn_end() {
    let reachable = reachable_signals();
    assert!(
        !reachable.contains(&BreakerSignal::InferredTurnEnd),
        "no v1 emitter writes an Inferred turn end — change this equation before the seam"
    );

    // And nothing the machine journals carries one either, across a run that beats repeatedly.
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    for signal in RuntimeSignal::ALL {
        harness.drive(Input::Signal { agent, signal });
        harness.drive(Input::Tick);
    }
    assert!(
        harness.records().all(|record| !matches!(
            record,
            Record::BreakerBeat { signals, .. } if signals.contains(&BreakerSignal::InferredTurnEnd)
        )),
        "no journaled beat names a signal with no emitter"
    );
}

/// The reserve must not be routed into the breaker. Invariant 3 rests on that: an `Inferred` event
/// may raise a rung but may never be the sole cause of `Stop`, and the reserve stays clear of invariant 3 only because its effect is `Exhausted` at gate 5.
///
/// **Why the check belongs in this crate.** The property is about *production*: no `BreakerSignal`
/// produced from a `TurnEnded` whose `usage` is `None`. `dudes-types` holds the structural half (a
/// variant carries no payload, so a figure cannot ride in on one) and `dudes-policy` the ladder
/// half (nothing but a signal climbs, and no signal the ladder can be handed is a cost) — but
/// neither crate *produces* a `BreakerSignal`. `RuntimeSignal::signal()` is the
/// workspace's only non-test construction site, so the wiring of an unpriced end into the ladder
/// would land here, in this crate. This test closes that flank.
///
/// Three claims, in descending strength:
///
/// 1. **The seam's shape**, from [`reachable_signals`]: the image is exactly the journaled signals
///    with a v1 emitter. This one is label-blind — no classification is consulted — but it is not
///    total over widenings: it catches a fifth `RuntimeSignal` only when the new signal does not
///    also grow `BreakerSignal::ALL`. That helper's table says which is which.
/// 2. **The classification**: every signal in that image is [`RaisedBy::Conduct`]. This is the
///    only arm that catches the case claim 1 misses — a widening with both `ALL`s updated — and
///    it catches it only when the new variant is labelled `Cost` by its own documentation. A
///    widening that is honestly `Conduct` is caught by neither, which is stated in
///    [`reachable_signals`] and measured rather than assumed.
/// 3. **Behaviour**: over a run that beats on every runtime signal there is, no journaled beat
///    names a cost-raised one.
///
/// **Claim 2 cannot fail on today's enum, and that is what a tripwire is.** No control is invented
/// for it: a "control" that did not run the real classifier would assert a capability it does not
/// establish. The evidence that it *can* fail is a mutation — a fifth `RuntimeSignal`
/// wired to a sixth, cost-raised `BreakerSignal` reddens it by name, while the identical mutation
/// classified `Conduct` stays green.
///
/// What is left uncovered is therefore **wider than mislabelling**, and the difference matters
/// enough to name: a variant wired and *mislabelled* here needs intent, and no test reads intent —
/// but a variant wired and **honestly labelled `Conduct`** needs no intent at all, and is equally
/// invisible. That is a gap the equation does not cover.
///
/// *Claim 3 has a live control*: the journaled set is asserted **equal** to the image rather than
/// merely free of cost-raised entries, so a run that wrote no beats at all — or one where the
/// filter matched nothing because the record shape changed — fails instead of passing vacuously.
#[test]
fn the_breaker_seam_carries_nothing_raised_by_what_a_turn_cost() {
    // 1 and 2: the seam's image, and its classification.
    let reachable = reachable_signals();
    let cost_raised: Vec<BreakerSignal> = reachable
        .iter()
        .copied()
        .filter(|signal| raised_by(*signal) == RaisedBy::Cost)
        .collect();
    assert!(
        cost_raised.is_empty(),
        "{cost_raised:?} reaches the breaker from a runtime and is raised by what a turn cost; \
         routing the reserve into the breaker is forbidden, so change this check before this seam"
    );

    // 3: and the machine journals no such signal either, over a run that beats on each of them.
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    for signal in RuntimeSignal::ALL {
        harness.drive(Input::Signal { agent, signal });
        harness.drive(Input::Tick);
    }
    let mut journaled: Vec<BreakerSignal> = harness
        .records()
        .filter_map(|record| match record {
            Record::BreakerBeat { signals, .. } => Some(signals.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    journaled.sort_unstable();
    let mut expected = reachable;
    expected.sort_unstable();
    assert_eq!(
        journaled, expected,
        "control: the run must journal exactly the signals it was given, so the absence below \
         covers a corpus that exists"
    );
    let journaled_costs: Vec<BreakerSignal> = journaled
        .into_iter()
        .filter(|signal| raised_by(*signal) == RaisedBy::Cost)
        .collect();
    assert!(
        journaled_costs.is_empty(),
        "{journaled_costs:?} was journaled into a beat and is raised by what a turn cost"
    );
}
