//! A turn is not issued once a ceiling is exceeded, and the refusal is **journaled**
//! rather than logged.
//!
//! # Where the spend comes from, and why forgetting it would be invisible
//!
//! `Supervisor` reads spend as `self.spend.get(&agent).copied().unwrap_or_default()` — so a daemon
//! that never pushed `Input::Spend` would admit against **zero spend, silently**, and this test
//! would pass for the wrong reason on the arm below the ceiling while never firing on the arm above
//! it. The daemon reads it from the redb budget projections (`spend_views`), for the UTC
//! day of the same clock reading the machine stamps entries with, and pushes it before every decide
//! — and, specifically, in front of the `TurnEnded` acknowledgement that *is* the decide which reads
//! it.
//!
//! # Which records carry the refusal
//!
//! Two records carry it, and the pair is the assertion: `Record::LoopDecided { decision: Exhausted { reason } }` is the
//! verdict, and `Record::BudgetDecision { allowed: false, .. }` is the admission check that
//! produced it (a refusal is the only `BudgetDecision` — an
//! admitted turn writes none, and the control below is what makes that half falsifiable).
//!
//! The reserve inputs are journaled separately on that record: `spent` remains observed spend,
//! `unknown_ends` is the projection count, and `per_turn_floor` is configuration. That separation
//! makes the admission quantity auditable without writing an estimate into `spent`.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::config::{ConfigError, load_budget};
use duded::ports::{Answered, Ask};
use duded::startup::{StartupStep, Witness};
use duded::{ExitCause, boot};
use dudes_journal::{Fsync, JournalOptions, Projections, Reader, replay};
use dudes_proto::rpc::goal::GoalSetParams;
use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData};
use dudes_proto::{
    AgentBudget, BudgetScope, Ceiling, Entry, ExhaustedReason, ExitStatus, GoalBudget,
    LoopDecision, Record, Spend, TurnOutcome,
};
use dudes_providers::stream::ScopedUsage;
use dudes_supervisor::Input;
use dudes_types::{AgentId, Provider, Timestamp, Tokens, Usage, Usd};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// A live hire's refusal when a token ceiling has no token reserve: it names the key.
const TOKEN_RESERVE_REFUSAL: &str = "a positive per-turn reserve is required: set \
     [budget.per_turn_floor] tokens above zero (dudes config set budget.per_turn_floor.tokens \
     <count>) and restart the daemon";

/// How long a correct daemon exit is given before `startup_exit` calls it a hang. Each of these
/// refusals does no I/O beyond writing to stderr, so a correct exit is near-instant; the margin
/// only has to outlast CPU contention from a machine under load, not the daemon's own work.
const DAEMON_EXIT_DEADLINE: Duration = Duration::from_secs(20);

fn no_agent_ceiling() -> AgentBudget {
    AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    }
}

#[test]
fn restored_ceiling_requires_same_unit_reserve_before_bind() {
    let fixture = Fixture::new();
    seed_agent(
        &fixture,
        AgentBudget {
            usd_per_day: Some(Usd::ZERO),
            tokens_per_day: Some(Tokens::ZERO),
        },
    );
    let before = fixture.records().len();
    let witness = Witness::default();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend::default();
    let host = SharedHost::new();
    let error = boot(
        &settings,
        seams(
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .err()
    .expect("both restored ceiling units need their own reserve");
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "a positive per-turn reserve is required: set [budget.per_turn_floor] usd and tokens \
             above zero (dudes config set budget.per_turn_floor.usd <dollars> \
             budget.per_turn_floor.tokens <count>)"
        ),
        "both keys, dollars first: {message}"
    );
    assert_eq!(
        witness.startup.steps(),
        vec![
            StartupStep::Lock,
            StartupStep::JournalOpen,
            StartupStep::Replay
        ],
        "refusal is after replay and before bind, reaper and first decide"
    );
    assert!(host.spawned().is_empty(), "no restored turn can spawn");
    assert_eq!(
        fixture.records().len(),
        before,
        "boot refusal appends nothing"
    );
    assert!(!settings.socket().exists(), "the control door never binds");

    let fixture = Fixture::new();
    seed_agent(
        &fixture,
        AgentBudget {
            usd_per_day: None,
            tokens_per_day: Some(Tokens::ZERO),
        },
    );
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend {
        usd: Usd::ZERO,
        tokens: Tokens::new(1),
        work_tokens: Tokens::new(1),
    };
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("a positive token reserve satisfies a token-only restored ceiling");
    let mut running = common::start(daemon);
    running.stop(ExitCause::Ended);
}

#[test]
fn live_ceiling_requires_same_unit_reserve_before_append() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend {
        usd: Usd::from_micros(1),
        ..Spend::default()
    };
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("ceiling-less state boots");
    let mut running = common::start(daemon);

    let token_hire = dudes_proto::rpc::agent::HireParams {
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: Some(Tokens::new(10)),
        },
        ..common::hire_params(fixture.state.path())
    };
    let before = fixture.records().len();
    let error = running
        .handle
        .ask(Ask::Hire(Box::new(token_hire)))
        .expect_err("the opposite-unit dollar reserve cannot satisfy a token ceiling");
    assert_eq!(gate(&error), RefusalGate::ReserveFloorRequired);
    assert_eq!(error.message, TOKEN_RESERVE_REFUSAL);
    assert_eq!(
        fixture.records().len(),
        before,
        "AgentHired was not appended"
    );

    let ceilingless = dudes_proto::rpc::agent::HireParams {
        budget: no_agent_ceiling(),
        ..common::hire_params(fixture.state.path())
    };
    let agent = match running
        .handle
        .ask(Ask::Hire(Box::new(ceilingless)))
        .expect("ceiling-less hire")
    {
        Answered::Hired(result) => result.agent,
        other => panic!("hire answered {other:?}"),
    };
    let mut goal = common::goal_params(agent, &[common::false_program()]);
    goal.budget = GoalBudget {
        usd: None,
        tokens: Some(Tokens::new(10)),
    };
    let before = fixture.records().len();
    let error = running
        .handle
        .ask(Ask::SetGoal(Box::new(goal)))
        .expect_err("token goal needs a token reserve");
    assert_eq!(gate(&error), RefusalGate::ReserveFloorRequired);
    assert_eq!(fixture.records().len(), before, "GoalSet was not appended");
    running.stop(ExitCause::Ended);

    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend {
        usd: Usd::ZERO,
        tokens: Tokens::new(1),
        work_tokens: Tokens::new(1),
    };
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("positive token reserve");
    let mut running = common::start(daemon);
    let params = dudes_proto::rpc::agent::HireParams {
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: Some(Tokens::new(10)),
        },
        ..common::hire_params(fixture.state.path())
    };
    running
        .handle
        .ask(Ask::Hire(Box::new(params)))
        .expect("same-unit token reserve admits the hire");
    assert_eq!(
        count(&fixture.records(), |record| matches!(
            record,
            Record::AgentHired { .. }
        )),
        1
    );
    running.stop(ExitCause::Ended);
}

fn no_goal_ceiling() -> GoalBudget {
    GoalBudget {
        usd: None,
        tokens: None,
    }
}

fn gate(error: &dudes_proto::rpc::RpcError) -> RefusalGate {
    assert_eq!(error.code, ErrorCode::Refused.code());
    serde_json::from_value::<RefusedData>(error.data.clone().expect("typed refusal data"))
        .expect("RefusedData")
        .gate
}

fn seed_agent(fixture: &Fixture, budget: AgentBudget) -> AgentId {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("fixed agent id");
    let options = JournalOptions {
        fsync: Fsync::Full,
        checkpoint_every: 0,
        ..JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open seed journal");
    writer
        .append(&Entry {
            at: Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::AgentHired {
                character: None,
                accent: None,
                id: agent,
                name: "Restored".to_owned(),
                role: "backend engineer".to_owned(),
                department: "engineering".to_owned(),
                provider: Provider::Claude,
                cwd: fixture.state.path().to_path_buf(),
                budget,
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
        })
        .expect("append restored agent");
    agent
}

/// A turn's usage, costing `usd`.
fn costing(usd: Usd) -> Usage {
    Usage {
        input: Tokens::ZERO,
        output: Tokens::ZERO,
        cache_read: Tokens::ZERO,
        cache_creation: Tokens::ZERO,
        usd: Some(usd),
    }
}

fn end_turn(
    handle: &duded::Handle,
    turn: dudes_types::TurnId,
    usage: Option<Usage>,
    outcome: TurnOutcome,
) {
    handle.input(end_payload(turn, usage, outcome));
}

fn end_payload(turn: dudes_types::TurnId, usage: Option<Usage>, outcome: TurnOutcome) -> Input {
    Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: usage.map(ScopedUsage::Turn),
        model_usage: BTreeMap::new(),
        outcome,
        last_message: None,
    }
}

fn await_spawned_turn(fixture: &Fixture) -> dudes_types::TurnId {
    let turn = fixture.await_turn();
    await_spawned(fixture, turn);
    turn
}

fn await_spawned(fixture: &Fixture, turn: dudes_types::TurnId) {
    fixture.await_record("the intended TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn).then_some(())
    });
}

/// Drive one turn to an end costing `spent` against a goal whose ceiling is `ceiling`, and return
/// the journal.
fn spend_against(ceiling: Usd, spent: Usd) -> Vec<Record> {
    let fixture = Fixture::new();
    let host = SharedHost::new();
    let daemon = boot(
        &fixture.settings(),
        // A predicate that never holds, so the loop's only reason to stop is the ceiling.
        seams(
            host.clone(),
            Arc::new(ScriptedRunner::scripted([common::fails()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params_with_budget(agent, &[common::false_program()], ceiling),
    );
    let turn = await_spawned_turn(&fixture);
    let issued_before = count(&fixture.records(), |record| {
        matches!(record, Record::TurnIssued { .. })
    });
    assert_eq!(
        issued_before, 1,
        "control: one turn was issued to begin with"
    );

    running.handle.input(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: Some(ScopedUsage::Turn(costing(spent))),
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    fixture.await_record("PredicateResult", |record| {
        matches!(record, Record::PredicateResult { .. }).then_some(())
    });

    if spent < ceiling {
        fixture.await_record("admitted second TurnIssued", |record| {
            matches!(record, Record::TurnIssued { turn: found, .. } if *found != turn).then_some(())
        });
    } else {
        fixture.await_record("goal budget refusal", |record| {
            matches!(record, Record::BudgetDecision { allowed: false, .. }).then_some(())
        });
    }

    running.stop(ExitCause::Ended);
    fixture.records()
}

/// Drive reported spend against a floor ceiling loaded through the same interface production uses.
fn spend_against_floor(source: &str, spent: Usd, expect_next_turn: bool) -> (Vec<Record>, usize) {
    let config_dir = tempfile::tempdir().expect("a config directory");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, source).expect("write the operator config");
    let budget = load_budget(&config_path).expect("load the operator budget");

    let fixture = Fixture::new();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = budget.floor_usd_per_day;
    let runtime_seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::fails()])),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&runtime_seams.children));
    let daemon = boot(&settings, runtime_seams).expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        // Both wider scopes stay above the one-micro-dollar spend, so only Floor can bind.
        common::goal_params_with_budget(agent, &[common::false_program()], Usd::from_dollars(5)),
    );
    let first = await_spawned_turn(&fixture);
    assert_eq!(host.spawned().len(), 1, "control: the first turn spawned");

    common::end_turn_with_native_payload(
        &fixture,
        &host,
        end_payload(first, Some(costing(spent)), TurnOutcome::Completed),
    );

    if expect_next_turn {
        fixture.await_record("second TurnIssued", |record| match record {
            Record::TurnIssued { turn, .. } if *turn != first => Some(()),
            _ => None,
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.spawned().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "the second durable turn was not spawned within 10s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let records = fixture.records();
            assert!(
                !records.iter().any(|record| matches!(
                    record,
                    Record::TurnIssued { turn, .. } if *turn != first
                )),
                "a second turn was issued at the reached floor ceiling: {:?}",
                kinds(&records)
            );
            if records.iter().any(|record| {
                matches!(
                    record,
                    Record::LoopDecided {
                        decision: LoopDecision::Exhausted {
                            reason: ExhaustedReason::FloorBudget
                        },
                        ..
                    }
                )
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "floor budget exhaustion did not reach the journal within 10s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    running.stop(ExitCause::Ended);
    (fixture.records(), host.spawned().len())
}

/// Load one complete document from a fresh path and return only the typed value under test.
fn load_floor(source: &str) -> Result<Option<Usd>, ConfigError> {
    let directory = tempfile::tempdir().expect("a config directory");
    let path = directory.path().join("config.toml");
    std::fs::write(&path, source).expect("write the operator config");
    load_budget(&path).map(|budget| budget.floor_usd_per_day)
}

/// Money is parsed from the source token exactly, with no float conversion or rounding.
#[test]
fn floor_budget_parses_exact_micro_dollars_and_refuses_unsupported_values() {
    let accepted = [
        ("0", 0),
        ("0.000001", 1),
        ("+0.0", 0),
        ("1", 1_000_000),
        ("1.000001", 1_000_001),
        ("+1.000_001", 1_000_001),
        ("50.0 # a trailing comment", 50_000_000),
        ("18446744073709.551615", u64::MAX),
    ];
    for (value, expected) in accepted {
        let source = format!("[budget]\nfloor_usd_per_day = {value}\n");
        assert_eq!(
            load_floor(&source)
                .expect("accepted exact decimal")
                .expect("a present key is a ceiling")
                .micros(),
            expected,
            "accepted value {value}"
        );
    }

    for value in [
        "-0.0",
        "-0.000001",
        "0.0000001",
        "1.0000000",
        "18446744073709.551616",
        "1e0",
        "0x1",
        "inf",
        "nan",
        "\"1.0\"",
        "true",
        "1979-05-27",
        "[]",
        "{}",
    ] {
        let source = format!("[budget]\nfloor_usd_per_day = {value}\n");
        assert!(load_floor(&source).is_err(), "refused value {value}");
    }

    assert_eq!(
        load_floor("").expect("an absent key"),
        None,
        "an absent key sets no floor ceiling; it does not fall back to a shipped number"
    );
    let directory = tempfile::tempdir().expect("a config directory");
    assert_eq!(
        load_budget(&directory.path().join("missing.toml"))
            .expect("an absent file")
            .floor_usd_per_day,
        None,
        "and neither does an absent file"
    );
    assert!(
        load_budget(directory.path()).is_err(),
        "a present path that cannot be read as a file is refused"
    );
}

/// A refused per-turn reserve names the one key that is wrong.
#[test]
fn a_refused_per_turn_reserve_names_its_key() {
    for (source, key) in [
        ("[budget.per_turn_floor]\nusd = -1\n", "usd"),
        ("[budget.per_turn_floor]\nusd = \"1\"\n", "usd"),
        ("[budget.per_turn_floor]\ntokens = -1\n", "tokens"),
        ("[budget.per_turn_floor]\nusd = 1\ntokens = 1.5\n", "tokens"),
    ] {
        let refused = load_floor(source).expect_err("an invalid reserve is refused");
        assert!(
            refused
                .to_string()
                .ends_with(&format!("has an invalid [budget.per_turn_floor] {key}")),
            "{source:?}: {refused}"
        );
    }
    assert!(
        load_floor("[budget.per_turn_floor]\nusd = 1\ntokens = 1\n").is_ok(),
        "control: both keys valid"
    );
}

/// Complete TOML parsing makes equivalent key spellings equal and malformed shapes fail closed.
#[test]
fn floor_budget_uses_semantic_toml_structure_and_refuses_invalid_documents() {
    for source in [
        "[budget]\nfloor_usd_per_day = 1.000001\n",
        "[\"budget\"]\n\"floor_usd_per_day\" = 1.000001\n",
        "['budget']\n'floor_usd_per_day' = 1.000001\n",
        "budget.floor_usd_per_day = 1.000001\n",
        "budget = { floor_usd_per_day = 1.000001 }\n",
        "# [budget]\n# floor_usd_per_day = 0\n[budget] # real\nfloor_usd_per_day = 1.000001 # trailing\n",
        "decoy = \"\"\"\n[budget]\nfloor_usd_per_day = 0\n\"\"\"\nother = '''\n[budget]\nfloor_usd_per_day = 0\n'''\n[budget]\nfloor_usd_per_day = 1.000001\n",
    ] {
        assert_eq!(
            load_floor(source)
                .expect("a semantic equivalent")
                .expect("a present key is a ceiling")
                .micros(),
            1_000_001
        );
    }

    for source in [
        "[budget]\nfloor_usd_per_day = 1\n\"floor_usd_per_day\" = 1\n",
        "[budget]\nfloor_usd_per_day = 1\n[budget]\n",
        "budget.floor_usd_per_day = 1\n[budget]\n",
        "budget = { floor_usd_per_day = 1 }\nbudget.extra = 2\n",
        "[[budget]]\nfloor_usd_per_day = 1\n",
        "budget = \"not a table\"\n",
        "[budget]\nfloor_usd_pre_day = 1\n",
        "[budget.extra]\nvalue = 1\n",
        "decoy = \"\"\"unterminated\n",
        "decoy = '''unterminated\n",
        "[\"budget]\nfloor_usd_per_day = 1\n",
        "[budget\nfloor_usd_per_day = 1\n",
        "[budget]\nfloor_usd_per_day =\n",
        "[unrelated]\nmalformed = [\n",
    ] {
        assert!(load_floor(source).is_err(), "invalid document was accepted");
    }

    for source in ["", "[budget]\n", "[unrelated]\nvalue = 1\n"] {
        assert_eq!(
            load_floor(source).expect("valid document without the owned key"),
            None
        );
    }
}

#[cfg(target_os = "macos")]
fn config_path(home: &Path, _xdg: &Path, _appdata: &Path) -> PathBuf {
    home.join("Library/Application Support/dudes/config.toml")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn config_path(_home: &Path, xdg: &Path, _appdata: &Path) -> PathBuf {
    xdg.join("dudes/config.toml")
}

#[cfg(windows)]
fn config_path(_home: &Path, _xdg: &Path, appdata: &Path) -> PathBuf {
    appdata.join("dudes/config.toml")
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_absolute_config_environment(
    command: &mut std::process::Command,
    home: &Path,
    _xdg: &Path,
    _appdata: &Path,
) {
    command.env("HOME", home);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_absolute_config_environment(
    command: &mut std::process::Command,
    home: &Path,
    xdg: &Path,
    _appdata: &Path,
) {
    command.env("HOME", home).env("XDG_CONFIG_HOME", xdg);
}

#[cfg(windows)]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_absolute_config_environment(
    command: &mut std::process::Command,
    _home: &Path,
    _xdg: &Path,
    appdata: &Path,
) {
    command.env("APPDATA", appdata);
}

/// Run the real daemon with one present config and capture a bounded startup result.
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn run_with_config(source: &str) -> (std::process::ExitStatus, String, PathBuf, bool, bool) {
    let root = tempfile::tempdir().expect("a process fixture");
    let home = root.path().join("home");
    let xdg = root.path().join("xdg");
    let appdata = root.path().join("appdata");
    let state = root.path().join("state-must-stay-absent");
    let runtime = root.path().join("runtime-must-stay-absent");
    let config = config_path(&home, &xdg, &appdata);
    std::fs::create_dir_all(config.parent().expect("the config parent"))
        .expect("create the config parent");
    std::fs::write(&config, source).expect("write the operator config");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .env("TMPDIR", root.path())
        .env("LANG", "C")
        .env("DUDES_STATE_DIR", &state)
        .env("DUDES_RUNTIME_DIR", &runtime);
    set_absolute_config_environment(&mut command, &home, &xdg, &appdata);
    let (status, stderr) = startup_exit(&mut command, "the present configuration");
    (status, stderr, config, state.exists(), runtime.exists())
}

#[derive(Clone, Copy, Debug)]
enum PoisonedRoot {
    Absent,
    Empty,
    Relative,
}

#[cfg(target_os = "macos")]
fn poisoned_config_path(cwd: &Path, root: PoisonedRoot) -> PathBuf {
    let home = match root {
        PoisonedRoot::Absent | PoisonedRoot::Empty => cwd.to_path_buf(),
        PoisonedRoot::Relative => cwd.join("relative-home"),
    };
    home.join("Library/Application Support/dudes/config.toml")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn poisoned_config_path(cwd: &Path, root: PoisonedRoot) -> PathBuf {
    match root {
        PoisonedRoot::Absent | PoisonedRoot::Empty => cwd.join(".config/dudes/config.toml"),
        PoisonedRoot::Relative => cwd.join("relative-xdg/dudes/config.toml"),
    }
}

#[cfg(windows)]
fn poisoned_config_path(cwd: &Path, root: PoisonedRoot) -> PathBuf {
    match root {
        PoisonedRoot::Absent | PoisonedRoot::Empty => cwd.join("dudes/config.toml"),
        PoisonedRoot::Relative => cwd.join("relative-appdata/dudes/config.toml"),
    }
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_poisoned_config_environment(command: &mut std::process::Command, root: PoisonedRoot) {
    match root {
        PoisonedRoot::Absent => command,
        PoisonedRoot::Empty => command.env("HOME", ""),
        PoisonedRoot::Relative => command.env("HOME", "relative-home"),
    };
}

#[cfg(all(unix, not(target_os = "macos")))]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_poisoned_config_environment(command: &mut std::process::Command, root: PoisonedRoot) {
    match root {
        PoisonedRoot::Absent => command,
        PoisonedRoot::Empty => command.env("HOME", "").env("XDG_CONFIG_HOME", ""),
        PoisonedRoot::Relative => command
            .env("HOME", "relative-home")
            .env("XDG_CONFIG_HOME", "relative-xdg"),
    };
}

#[cfg(windows)]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness configures the daemon process; host_seam.rs separately guards src/"
)]
fn set_poisoned_config_environment(command: &mut std::process::Command, root: PoisonedRoot) {
    match root {
        PoisonedRoot::Absent => command,
        PoisonedRoot::Empty => command.env("APPDATA", ""),
        PoisonedRoot::Relative => command.env("APPDATA", "relative-appdata"),
    };
}

/// Start the real daemon with no absolute config root and a valid config planted under cwd.
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn run_with_poisoned_root(root_kind: PoisonedRoot) -> (String, bool, bool) {
    let root = tempfile::tempdir().expect("a process fixture");
    let state = root.path().join("state-must-stay-absent");
    let runtime = root.path().join("runtime-must-stay-absent");
    let planted = poisoned_config_path(root.path(), root_kind);
    std::fs::create_dir_all(planted.parent().expect("the config parent"))
        .expect("create the planted config parent");
    std::fs::write(planted, "[budget]\nfloor_usd_per_day = 999999.000000\n")
        .expect("plant a valid config under cwd");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .current_dir(root.path())
        .env("TMPDIR", root.path())
        .env("LANG", "C")
        .env("DUDES_STATE_DIR", &state)
        .env("DUDES_RUNTIME_DIR", &runtime);
    set_poisoned_config_environment(&mut command, root_kind);
    let (status, stderr) = startup_exit(&mut command, &format!("a {root_kind:?} config root"));
    assert!(!status.success(), "the invalid config root is refused");
    (stderr, state.exists(), runtime.exists())
}

/// Start the daemon, require it to exit within [`DAEMON_EXIT_DEADLINE`], and return its status and
/// standard error.
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn startup_exit(
    command: &mut std::process::Command,
    accepted: &str,
) -> (std::process::ExitStatus, String) {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the daemon binary starts");
    let deadline = Instant::now() + DAEMON_EXIT_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the daemon") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the daemon accepted {accepted} and did not exit within {DAEMON_EXIT_DEADLINE:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("captured standard error")
        .read_to_string(&mut stderr)
        .expect("read standard error");
    (status, stderr)
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn an_unavailable_home_refuses_the_default_runtime_directory() {
    for home in [None, Some(""), Some("relative-home")] {
        let root = tempfile::tempdir().expect("a process fixture");
        let state = root.path().join("state-must-stay-absent");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
        command
            .env_clear()
            .current_dir(root.path())
            .env("TMPDIR", root.path())
            .env("LANG", "C")
            .env("DUDES_STATE_DIR", &state)
            .env("XDG_CONFIG_HOME", root.path().join("xdg"))
            .env("APPDATA", root.path().join("appdata"));
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let (status, stderr) = startup_exit(
            &mut command,
            &format!("HOME {home:?} with no DUDES_RUNTIME_DIR"),
        );
        assert!(!status.success(), "HOME {home:?} is refused");
        let missing = if cfg!(windows) {
            "HOME is unavailable; set an absolute HOME or, with HOME unset, an absolute USERPROFILE or DUDES_RUNTIME_DIR"
        } else {
            "HOME is unavailable; set an absolute HOME or DUDES_RUNTIME_DIR"
        };
        assert!(
            stderr.contains(missing),
            "the refusal names the override: {stderr:?}"
        );
        assert!(
            !root.path().join(".local").exists() && !root.path().join("relative-home").exists(),
            "cwd stood in for HOME {home:?}"
        );
        assert!(!state.exists(), "state was not created before refusal");
    }
}

/// Missing, empty and relative platform roots never make cwd an operator config authority.
#[test]
fn invalid_config_roots_refuse_before_cwd_or_persistent_state() {
    for root in [
        PoisonedRoot::Absent,
        PoisonedRoot::Empty,
        PoisonedRoot::Relative,
    ] {
        let (stderr, state_exists, runtime_exists) = run_with_poisoned_root(root);
        assert!(
            stderr.contains("operator configuration root is unavailable"),
            "the diagnostic is generic: {stderr:?}"
        );
        assert!(
            !stderr.contains("relative-") && !stderr.contains("999999"),
            "the diagnostic contains no environment or TOML value: {stderr:?}"
        );
        assert!(!state_exists, "state was not created before refusal");
        assert!(
            !runtime_exists,
            "runtime state was not created before refusal"
        );
    }
}

/// A valid absolute platform root with no configuration file sets no floor ceiling and still
/// reaches daemon boot.
#[test]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn absolute_config_root_with_missing_file_has_no_floor_ceiling_and_boots() {
    let root = tempfile::tempdir().expect("a process fixture");
    let home = root.path().join("home");
    let xdg = root.path().join("xdg");
    let appdata = root.path().join("appdata");
    let state = root.path().join("state");
    let runtime = root.path().join("runtime");
    let config = config_path(&home, &xdg, &appdata);
    assert_eq!(
        load_budget(&config)
            .expect("a missing file below a valid absolute root")
            .floor_usd_per_day,
        None
    );

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .env("TMPDIR", root.path())
        .env("LANG", "C")
        .env("DUDES_STATE_DIR", &state)
        .env("DUDES_RUNTIME_DIR", &runtime)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    set_absolute_config_environment(&mut command, &home, &xdg, &appdata);
    let mut child = command.spawn().expect("the daemon binary starts");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !state.join(duded::boot::JOURNAL_DIR).exists() {
        assert!(
            child.try_wait().expect("poll the daemon").is_none(),
            "the valid absolute-root control exited before journal open"
        );
        assert!(
            Instant::now() < deadline,
            "the valid absolute-root control did not open the journal within 10s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("stop the positive-control daemon");
    child.wait().expect("reap the positive-control daemon");
}

/// The XDG convention ignores a relative override and falls back to an absolute HOME.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the daemon binary; host_seam.rs separately guards src/"
)]
fn relative_xdg_root_falls_back_to_absolute_home() {
    let root = tempfile::tempdir().expect("a process fixture");
    let home = root.path().join("home");
    let fallback = home.join(".config/dudes/config.toml");
    std::fs::create_dir_all(fallback.parent().expect("the fallback parent"))
        .expect("create the fallback parent");
    std::fs::write(&fallback, "[budget]\nfloor_usd_per_day = -0.0\n")
        .expect("write the fallback config");
    let planted = root.path().join("relative-xdg/dudes/config.toml");
    std::fs::create_dir_all(planted.parent().expect("the planted parent"))
        .expect("create the planted parent");
    std::fs::write(&planted, "[budget]\nfloor_usd_per_day = 999999\n")
        .expect("write the planted config");
    let state = root.path().join("state-must-stay-absent");
    let runtime = root.path().join("runtime-must-stay-absent");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duded"));
    command
        .env_clear()
        .current_dir(root.path())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "relative-xdg")
        .env("TMPDIR", root.path())
        .env("LANG", "C")
        .env("DUDES_STATE_DIR", &state)
        .env("DUDES_RUNTIME_DIR", &runtime);
    let (status, stderr) = startup_exit(
        &mut command,
        "the relative XDG root instead of absolute HOME",
    );
    assert!(!status.success());
    assert!(stderr.contains(&fallback.display().to_string()));
    assert!(!stderr.contains("relative-xdg") && !stderr.contains("999999"));
    assert!(!state.exists() && !runtime.exists());
}

/// A bad present value exits before persistent state or any child-owning host is constructed.
#[test]
fn invalid_floor_config_exits_before_state_or_journal() {
    let private_value = "DO_NOT_ECHO_THIS_VALUE";
    let source = format!("unrelated = \"{private_value}\"\n[budget]\nfloor_usd_per_day = -0.0\n");
    let (status, stderr, config, state_exists, runtime_exists) = run_with_config(&source);

    assert!(!status.success(), "the present invalid value is refused");
    assert!(
        stderr.contains(&config.display().to_string()),
        "the diagnostic names the config path: {stderr:?}"
    );
    assert!(
        stderr.contains("[budget] floor_usd_per_day"),
        "the diagnostic names the refused setting: {stderr:?}"
    );
    assert!(
        !stderr.contains(private_value) && !stderr.contains("-0.0"),
        "the diagnostic contains no TOML values: {stderr:?}"
    );
    assert!(!state_exists, "state was not created before refusal");
    assert!(
        !runtime_exists,
        "runtime state was not created before refusal"
    );
}

/// Representative parser and value failures all stop at the same value-free pre-boot seam.
#[test]
fn representative_invalid_floor_configs_fail_closed_without_echoing_values() {
    let private_value = "DO_NOT_ECHO_THIS_VALUE";
    for (invalid, forbidden) in [
        ("unterminated = \"\n", "unterminated"),
        ("[budget]\nfloor_usd_pre_day = 1\n", "floor_usd_pre_day"),
        ("[budget]\nfloor_usd_per_day = -0.0\n", "-0.0"),
        ("[budget]\nfloor_usd_per_day = \"not money\"\n", "not money"),
        ("[budget]\nfloor_usd_per_day = 0.0000001\n", "0.0000001"),
        (
            "[budget]\nfloor_usd_per_day = 18446744073709.551616\n",
            "18446744073709.551616",
        ),
    ] {
        let source = format!("unrelated = \"{private_value}\"\n{invalid}");
        let (status, stderr, config, state_exists, runtime_exists) = run_with_config(&source);
        assert!(!status.success(), "the present invalid config is refused");
        assert!(
            stderr.contains(&config.display().to_string()),
            "the diagnostic names the config path: {stderr:?}"
        );
        assert!(
            stderr.contains("TOML document") || stderr.contains("[budget] floor_usd_per_day"),
            "the diagnostic names a generic cause: {stderr:?}"
        );
        assert!(
            !stderr.contains(private_value) && !stderr.contains(forbidden),
            "the diagnostic contains no TOML values or snippets: {stderr:?}"
        );
        assert!(!state_exists, "state was not created before refusal");
        assert!(
            !runtime_exists,
            "runtime state was not created before refusal"
        );
    }
}

/// Equality refuses before a next issue.
#[test]
fn floor_ceiling_arm_exhausts_before_issue() {
    let spent = Usd::from_micros(1);
    let (reached, reached_spawns) =
        spend_against_floor("[budget]\nfloor_usd_per_day = 0.000001\n", spent, false);

    assert_eq!(reached_spawns, 1, "no second child was spawned at equality");
    assert_eq!(
        count(&reached, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        1,
        "no second turn was issued at equality: {:?}",
        kinds(&reached)
    );
    let refusals: Vec<&Record> = reached
        .iter()
        .filter(|record| matches!(record, Record::BudgetDecision { .. }))
        .collect();
    assert_eq!(refusals.len(), 1, "exactly one refusal: {refusals:?}");
    let Record::BudgetDecision {
        allowed,
        spent,
        ceiling,
        ..
    } = refusals[0]
    else {
        unreachable!("filtered to BudgetDecision")
    };
    assert!(!*allowed);
    assert_eq!(
        *spent,
        Spend {
            usd: Usd::from_micros(1),
            tokens: Tokens::ZERO,
            work_tokens: Tokens::ZERO,
        }
    );
    assert_eq!(
        *ceiling,
        Ceiling {
            scope: BudgetScope::Floor,
            usd: Some(Usd::from_micros(1)),
            tokens: None,
        }
    );
    let refusal = reached
        .iter()
        .position(|record| matches!(record, Record::BudgetDecision { .. }))
        .expect("the refusal index");
    assert!(
        matches!(
            reached.get(refusal + 1),
            Some(Record::LoopDecided {
                decision: LoopDecision::Exhausted {
                    reason: ExhaustedReason::FloorBudget
                },
                ..
            })
        ),
        "the floor refusal immediately precedes its exhausted verdict: {:?}",
        kinds(&reached)
    );
}

/// **The retired default, asserted on behaviour rather than on the loader.** With no `[budget]`
/// key at all, a turn that reported $51 of spend is followed by another — where the shipped $50
/// default would have refused it.
///
/// **$51 is chosen against three numbers, not one.** It is above the retired `$50` default, above
/// `Usd::ZERO` — so neither a kept default nor a zero-coercing read of the absent ceiling could
/// reach the assertion — and far below this arm's own goal and agent ceilings, which are set to
/// `$1000` here precisely so the floor is the only scope that could bind. `spend_against_floor`
/// cannot be reused: its ceilings are `$5` and `$10`, so any spend large enough to clear the old
/// default would be refused by the goal scope first and this test would pass for the wrong reason.
///
/// The two floor tests below drive a *present* ceiling through the same path and must stay green.
/// Together the three say: a configured floor still binds, and an unconfigured one never did.
#[test]
fn an_absent_floor_ceiling_issues_a_turn_the_old_default_would_have_refused() {
    let config_dir = tempfile::tempdir().expect("a config directory");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, "").expect("write an empty operator config");
    let budget = load_budget(&config_path).expect("load the operator budget");
    assert_eq!(
        budget.floor_usd_per_day, None,
        "the fixture is a configuration with no floor ceiling at all"
    );

    let fixture = Fixture::new();
    let host = SharedHost::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = budget.floor_usd_per_day;
    let daemon = boot(
        &settings,
        seams(
            host.clone(),
            Arc::new(ScriptedRunner::scripted([common::fails()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    // Both wider scopes far above the spend, so only the floor could bind — and it is absent.
    let agent = match running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(
            dudes_proto::rpc::agent::HireParams {
                budget: dudes_proto::AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(1_000)),
                    tokens_per_day: None,
                },
                ..common::hire_params(fixture.state.path())
            },
        )))
        .expect("the hire")
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    };
    common::set_goal(
        &running.handle,
        common::goal_params_with_budget(
            agent,
            &[common::false_program()],
            Usd::from_dollars(1_000),
        ),
    );
    let first = await_spawned_turn(&fixture);
    assert_eq!(host.spawned().len(), 1, "control: the first turn spawned");

    running.handle.input(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn: first,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: Some(ScopedUsage::Turn(costing(Usd::from_dollars(51)))),
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });

    fixture.await_record("second TurnIssued", |record| match record {
        Record::TurnIssued { turn, .. } if *turn != first => Some(()),
        _ => None,
    });

    running.stop(ExitCause::Ended);
    let records = fixture.records();
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        2,
        "$51 spent with no floor ceiling issues the next turn: {:?}",
        kinds(&records)
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::BudgetDecision { .. })),
        "and records no refusal in any scope: {:?}",
        kinds(&records)
    );
}

/// Positive control: changing only the ceiling admits and spawns the same next turn.
#[test]
fn higher_floor_ceiling_control_issues_next_turn() {
    let (records, spawns) = spend_against_floor(
        "[budget]\nfloor_usd_per_day = 0.000002\n",
        Usd::from_micros(1),
        true,
    );
    assert_eq!(spawns, 2, "the higher ceiling spawned the next turn");
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        2,
        "the higher ceiling issued the next turn: {:?}",
        kinds(&records)
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::BudgetDecision { .. })),
        "the higher ceiling records no refusal: {:?}",
        kinds(&records)
    );
}

/// A ceiling that has been exceeded refuses the next turn, and the refusal is a record.
#[test]
fn a_turn_is_not_issued_once_the_goal_ceiling_is_exceeded() {
    // A goal that may spend one dollar, and a turn that spent five.
    let records = spend_against(Usd::from_dollars(1), Usd::from_dollars(5));

    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        1,
        "no second turn was issued after the ceiling was passed: {:?}",
        kinds(&records)
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted { .. },
                ..
            }
        )),
        "and the refusal is journaled, not logged: {:?}",
        kinds(&records)
    );
    // The stricter reading: the admission check is a record of
    // its own, carrying the ceiling that bound and the spend that reached it.
    let refusals: Vec<&Record> = records
        .iter()
        .filter(|record| matches!(record, Record::BudgetDecision { .. }))
        .collect();
    assert!(
        matches!(
            refusals.as_slice(),
            [Record::BudgetDecision {
                allowed: false,
                ceiling: Ceiling {
                    scope: BudgetScope::Goal,
                    ..
                },
                ..
            }]
        ),
        "exactly one BudgetDecision, refusing on the goal ceiling: {refusals:?}"
    );
    // And in lsn order on the disk, every check precedes the verdict it produced. Over every
    // pair, not the first: the tick's sweep re-decides an exhausted agent through
    // the same `decide()`, so a daemon run can journal several decisions and a comparison of first
    // positions would pair one agent's refusal with another's verdict.
    let order = kinds(&records);
    for (index, kind) in order.iter().enumerate() {
        assert!(
            *kind != dudes_proto::RecordKind::BudgetDecision
                || order.get(index + 1) == Some(&dudes_proto::RecordKind::LoopDecided),
            "the BudgetDecision at {index} is not immediately followed by its verdict: {order:?}"
        );
    }
}

/// The control for the test above, without which the test above passes on a daemon that issues
/// nothing at all: the identical fixture **below** the ceiling issues a second turn and journals no
/// refusal.
#[test]
fn the_identical_fixture_below_the_ceiling_issues_another_turn() {
    // The same goal ceiling, and a turn that spent a cent.
    let records = spend_against(Usd::from_dollars(1), Usd::from_cents(1));

    assert!(
        count(&records, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )) >= 2,
        "control: below the ceiling the loop keeps going: {:?}",
        kinds(&records)
    );
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted { .. },
                ..
            }
        )),
        "and nothing was refused: {:?}",
        kinds(&records)
    );
    // An admitted turn's scopes, ceilings and spends are already on the budget gate's
    // passing note. A second record per admitted turn would be a copy of a fact, on the busiest
    // path there is.
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::BudgetDecision { .. })),
        "so an admission is not journaled at all: {:?}",
        kinds(&records)
    );
}

/// How many records satisfy `f`.
fn count(records: &[Record], f: impl Fn(&Record) -> bool) -> usize {
    records.iter().filter(|record| f(record)).count()
}

/// Record kinds, for a failure message.
fn kinds(records: &[Record]) -> Vec<dudes_proto::RecordKind> {
    records.iter().map(Record::kind).collect()
}

/// Drive reported **token** usage against a goal whose ceilings are `budget`, through the daemon.
///
/// The sibling of [`spend_against`] one unit over. The agent is hired with a dollar ceiling far
/// above anything reported and no token ceiling of its own, and the floor is absent, so the goal's
/// own ceilings are the only ones that can bind — which is what makes the scope in the assertions
/// mean something.
fn tokens_against(budget: GoalBudget, tokens: u64) -> Vec<Record> {
    tokens_against_inner(budget, tokens, true)
}

/// As [`tokens_against`], for a ceiling that refuses **before** the first turn — where there is no
/// turn to report usage for, so waiting for one would hang rather than fail.
fn tokens_refused_before_any_turn(budget: GoalBudget) -> Vec<Record> {
    tokens_against_inner(budget, 0, false)
}

fn tokens_against_inner(budget: GoalBudget, tokens: u64, expect_a_turn: bool) -> Vec<Record> {
    let expect_next_turn = budget
        .tokens
        .is_some_and(|ceiling| Tokens::new(tokens) < ceiling);
    let fixture = Fixture::new();
    let host = SharedHost::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    let daemon = boot(
        &settings,
        // A predicate that never holds, so the loop's only reason to stop is a ceiling.
        seams(
            host.clone(),
            Arc::new(ScriptedRunner::scripted([common::fails()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = match running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(
            dudes_proto::rpc::agent::HireParams {
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(1_000)),
                    tokens_per_day: None,
                },
                ..common::hire_params(fixture.state.path())
            },
        )))
        .expect("the hire")
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    };
    common::set_goal(
        &running.handle,
        GoalSetParams {
            budget,
            ..common::goal_params(agent, &[common::false_program()])
        },
    );
    if expect_a_turn {
        let turn = await_spawned_turn(&fixture);
        running.handle.input(Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(ScopedUsage::Turn(Usage {
                input: Tokens::new(tokens),
                output: Tokens::ZERO,
                cache_read: Tokens::ZERO,
                cache_creation: Tokens::ZERO,
                usd: Some(Usd::ZERO),
            })),
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        });
        fixture.await_record("PredicateResult", |record| {
            matches!(record, Record::PredicateResult { .. }).then_some(())
        });
        if expect_next_turn {
            fixture.await_record("admitted second TurnIssued", |record| {
                matches!(record, Record::TurnIssued { turn: found, .. } if *found != turn)
                    .then_some(())
            });
        } else {
            fixture.await_record("goal token refusal", |record| {
                matches!(record, Record::BudgetDecision { allowed: false, .. }).then_some(())
            });
        }
    } else {
        fixture.await_record("BudgetDecision", |record| {
            matches!(record, Record::BudgetDecision { allowed: false, .. }).then_some(())
        });
    }

    running.stop(ExitCause::Ended);
    fixture.records()
}

/// **`dudes goal --tokens N` binds at the daemon, in the goal scope** — the journal half of the
/// `--tokens` flag, which is otherwise pinned only at the parser and in pure policy.
///
/// A goal capped at 1 000 tokens whose turn reported 1 000 records **exactly one**
/// `BudgetDecision{scope: Goal}` refusal and issues no second turn. The scope is asserted, not just
/// the refusal: the agent's dollar ceiling is $1 000 and the floor is absent, so a refusal landing
/// anywhere else would mean the token cap was not what bound.
///
/// **Control 1**, one input apart: the same goal under a token ceiling *above* its usage issues the
/// next turn and journals no refusal at all. Without it, a daemon that refused every second turn
/// would satisfy the subject.
///
/// **Control 2**, the other unit in the same scope: `--usd 0` refuses before the first turn — and
/// on `BudgetScope::Goal`, not the floor or the agent. That is the *zero* row
/// reaching the daemon through the goal's own dollar field, which is the field that was made
/// optional; an implementation that had quietly moved the zero check to another scope would pass
/// every other arm in this file.
#[test]
fn a_goal_token_ceiling_binds_at_the_daemon_in_the_goal_scope() {
    let reached = tokens_against(
        GoalBudget {
            usd: None,
            tokens: Some(Tokens::new(1_000)),
        },
        1_000,
    );
    assert_eq!(
        count(&reached, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        1,
        "no second turn was issued at the token ceiling: {:?}",
        kinds(&reached)
    );
    let refusals: Vec<&Record> = reached
        .iter()
        .filter(|record| matches!(record, Record::BudgetDecision { allowed: false, .. }))
        .collect();
    assert!(
        matches!(
            refusals.as_slice(),
            [Record::BudgetDecision {
                ceiling: Ceiling {
                    scope: BudgetScope::Goal,
                    usd: None,
                    tokens: Some(_),
                },
                ..
            }]
        ),
        "exactly one refusal, on the goal's token ceiling, carrying no dollar figure: {refusals:?}"
    );

    // Control 1: the same goal one input apart, capped above its usage.
    let under = tokens_against(
        GoalBudget {
            usd: None,
            tokens: Some(Tokens::new(1_001)),
        },
        1_000,
    );
    assert_eq!(
        count(&under, |record| matches!(record, Record::TurnIssued { .. })),
        2,
        "one token below the cap issues the next turn: {:?}",
        kinds(&under)
    );
    assert!(
        !under
            .iter()
            .any(|record| matches!(record, Record::BudgetDecision { .. })),
        "and journals no refusal: {:?}",
        kinds(&under)
    );

    // Control 2: the other unit, same scope, the *zero* state of the absent/zero/set ceiling.
    let zero = tokens_refused_before_any_turn(GoalBudget {
        usd: Some(Usd::ZERO),
        tokens: None,
    });
    assert!(
        !zero
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "a zero goal ceiling refuses before the first turn is issued, not after it ran: {:?}",
        kinds(&zero)
    );
    let refusals: Vec<&Record> = zero
        .iter()
        .filter(|record| matches!(record, Record::BudgetDecision { allowed: false, .. }))
        .collect();
    assert!(
        matches!(
            refusals.as_slice(),
            [
                Record::BudgetDecision {
                    ceiling: Ceiling {
                        scope: BudgetScope::Goal,
                        usd: Some(Usd::ZERO),
                        ..
                    },
                    ..
                },
                ..
            ]
        ),
        "`--usd 0` refuses on the goal's own zero ceiling, not the floor's or the agent's: \
         {refusals:?}"
    );
}

#[test]
fn zero_and_no_ceiling_controls_keep_units_independent() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend::default();
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("zero reserve is valid with no ceiling");
    let mut running = common::start(daemon);
    let hire = dudes_proto::rpc::agent::HireParams {
        budget: no_agent_ceiling(),
        ..common::hire_params(fixture.state.path())
    };
    let agent = match running.handle.ask(Ask::Hire(Box::new(hire))).expect("hire") {
        Answered::Hired(result) => result.agent,
        other => panic!("hire answered {other:?}"),
    };
    let mut goal = common::goal_params(agent, &[common::false_program()]);
    goal.budget = no_goal_ceiling();
    running
        .handle
        .ask(Ask::SetGoal(Box::new(goal)))
        .expect("ceiling-less goal");
    running.stop(ExitCause::Ended);

    for (budget, floor, wanted) in [
        (
            AgentBudget {
                usd_per_day: Some(Usd::ZERO),
                tokens_per_day: None,
            },
            Spend {
                usd: Usd::ZERO,
                tokens: Tokens::new(1),
                work_tokens: Tokens::new(1),
            },
            "a positive per-turn reserve is required: set [budget.per_turn_floor] usd above zero \
             (dudes config set budget.per_turn_floor.usd <dollars>) and restart the daemon",
        ),
        (
            AgentBudget {
                usd_per_day: None,
                tokens_per_day: Some(Tokens::ZERO),
            },
            Spend {
                usd: Usd::from_micros(1),
                tokens: Tokens::ZERO,
                work_tokens: Tokens::ZERO,
            },
            TOKEN_RESERVE_REFUSAL,
        ),
    ] {
        let fixture = Fixture::new();
        let mut settings = fixture.settings();
        settings.floor_usd_per_day = None;
        settings.per_turn_floor = floor;
        let daemon = boot(
            &settings,
            seams(
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("no restored ceiling");
        let mut running = common::start(daemon);
        let params = dudes_proto::rpc::agent::HireParams {
            budget,
            ..common::hire_params(fixture.state.path())
        };
        let before = fixture.records().len();
        let error = running
            .handle
            .ask(Ask::Hire(Box::new(params)))
            .expect_err("present zero is a ceiling and opposite-unit reserve is insufficient");
        assert_eq!(gate(&error), RefusalGate::ReserveFloorRequired);
        assert_eq!(error.message, wanted);
        assert_eq!(fixture.records().len(), before);
        running.stop(ExitCause::Ended);
    }
}

#[test]
fn unknown_ends_and_floor_rebuild_from_journal_and_config() {
    let config_dir = tempfile::tempdir().expect("config directory");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[budget]\nper_turn_floor = { usd = 0.000003, tokens = 7 }\n",
    )
    .expect("write reserve config");
    let budget = load_budget(&config_path).expect("load reserve config");
    assert_eq!(
        budget.per_turn_floor,
        Spend {
            usd: Usd::from_micros(3),
            tokens: Tokens::new(7),
            work_tokens: Tokens::new(7),
        }
    );

    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = budget.per_turn_floor;
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let runtime_seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::fails(), common::fails()])),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&runtime_seams.children));
    let daemon = boot(&settings, runtime_seams).expect("boot with configured reserve");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params_with_budget(agent, &[common::false_program()], Usd::from_micros(4)),
    );
    let first = await_spawned_turn(&fixture);
    common::end_turn_with_native_payload(
        &fixture,
        &host,
        end_payload(
            first,
            Some(costing(Usd::from_micros(1))),
            TurnOutcome::Completed,
        ),
    );
    let second = fixture.await_record("second TurnIssued", |record| match record {
        Record::TurnIssued { turn, .. } if *turn != first => Some(*turn),
        _ => None,
    });
    await_spawned(&fixture, second);
    common::end_turn_with_native_payload(
        &fixture,
        &host,
        end_payload(second, None, TurnOutcome::Vanished),
    );
    fixture.await_record("reserved budget refusal", |record| match record {
        Record::BudgetDecision {
            spent,
            unknown_ends,
            per_turn_floor,
            ..
        } if spent.usd == Usd::from_micros(1)
            && *unknown_ends == 1
            && *per_turn_floor == budget.per_turn_floor =>
        {
            Some(())
        }
        _ => None,
    });
    running.stop(ExitCause::Ended);

    let projection_path = fixture.state.path().join(duded::boot::PROJECTION_FILE);
    let projected = Projections::open(&projection_path).expect("open live projection");
    let day = Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
    let before = projected
        .spend_views(agent, goal, day)
        .expect("read live budget view");
    assert_eq!(before.goal.usd, Usd::from_micros(1));
    assert_eq!(before.goal_unknown_ends, 1);
    assert_eq!(before.agent_unknown_ends_today, 1);
    assert_eq!(before.floor_unknown_ends_today, 1);
    drop(projected);

    std::fs::remove_file(&projection_path).expect("drop the derived projection");
    let mut rebuilt = Projections::open(&projection_path).expect("open empty projection");
    let report =
        replay(&Reader::new(&fixture.journal_dir()), &mut rebuilt).expect("replay journal");
    assert!(
        report.applied > 0,
        "positive control: replay applied records"
    );
    assert_eq!(
        rebuilt.spend_views(agent, goal, day).expect("rebuilt view"),
        before,
        "journal alone rebuilds spend and unknown-end counts"
    );
}

/// Each unit of `[budget.per_turn_floor]` the operator leaves unset is $0.25 or
/// 20,000 tokens, whether the table is absent or names only the other unit. An explicit zero stays
/// zero.
#[test]
fn an_unset_per_turn_reserve_defaults_in_each_unit() {
    let spend = |micros: u64, tokens: u64| Spend {
        usd: Usd::from_micros(micros),
        tokens: Tokens::new(tokens),
        work_tokens: Tokens::new(tokens),
    };
    assert_eq!(Spend::DEFAULT_PER_TURN_FLOOR, spend(250_000, 20_000));
    for (source, wanted) in [
        ("", spend(250_000, 20_000)),
        ("[budget]\nfloor_usd_per_day = 5\n", spend(250_000, 20_000)),
        (
            "[budget.per_turn_floor]\nusd = 0.5\n",
            spend(500_000, 20_000),
        ),
        ("[budget.per_turn_floor]\ntokens = 7\n", spend(250_000, 7)),
        (
            "[budget.per_turn_floor]\nusd = 0\ntokens = 0\n",
            spend(0, 0),
        ),
    ] {
        let directory = tempfile::tempdir().expect("a config directory");
        let path = directory.path().join("config.toml");
        std::fs::write(&path, source).expect("write the operator config");
        assert_eq!(
            load_budget(&path).expect("a valid file").per_turn_floor,
            wanted,
            "{source:?}"
        );
    }
}

/// A ceiling beside unset reserves: the daemon boots and issues the first turn, and an end whose
/// usage it never saw is priced at the default reserve, so the next turn is refused once that
/// charge meets the ceiling. A floor ceiling of $0.25 binds in dollars, and a goal ceiling of 20,000
/// tokens in tokens.
#[test]
fn an_unset_reserve_prices_an_unobserved_end_at_the_default() {
    for (source, goal_tokens, scope) in [
        (
            "[budget]\nfloor_usd_per_day = 0.25\n",
            None,
            BudgetScope::Floor,
        ),
        ("", Some(Tokens::new(20_000)), BudgetScope::Goal),
    ] {
        let directory = tempfile::tempdir().expect("a config directory");
        let path = directory.path().join("config.toml");
        std::fs::write(&path, source).expect("write the operator config");
        let budget = load_budget(&path).expect("load the operator budget");
        let fixture = Fixture::new();
        let mut settings = fixture.settings();
        settings.floor_usd_per_day = budget.floor_usd_per_day;
        settings.per_turn_floor = budget.per_turn_floor;
        let daemon = boot(
            &settings,
            seams(
                SharedHost::new(),
                Arc::new(ScriptedRunner::scripted([common::fails()])),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("a ceiling beside unset reserves boots");
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let mut goal = common::goal_params(agent, &[common::false_program()]);
        goal.budget = GoalBudget {
            usd: None,
            tokens: goal_tokens,
        };
        common::set_goal(&running.handle, goal);
        let first = await_spawned_turn(&fixture);
        end_turn(&running.handle, first, None, TurnOutcome::Vanished);
        let (ceiling, unknown_ends, per_turn_floor) =
            fixture.await_record("a refused BudgetDecision", |record| match record {
                Record::BudgetDecision {
                    allowed: false,
                    ceiling,
                    unknown_ends,
                    per_turn_floor,
                    ..
                } => Some((*ceiling, *unknown_ends, *per_turn_floor)),
                _ => None,
            });
        assert_eq!(ceiling.scope, scope, "{source:?}");
        assert_eq!(unknown_ends, 1, "{source:?}");
        assert_eq!(per_turn_floor, Spend::DEFAULT_PER_TURN_FLOOR, "{source:?}");
        assert_eq!(
            count(&fixture.records(), |record| matches!(
                record,
                Record::TurnIssued { .. }
            )),
            1,
            "{source:?}: no turn after the reserved charge meets the ceiling"
        );
        running.stop(ExitCause::Ended);
    }
}
