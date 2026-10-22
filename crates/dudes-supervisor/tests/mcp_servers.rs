//! Declared MCP servers at the supervisor: every launch admits an agent's recorded servers again
//! against the definitions this run loaded, and a managed turn's report of the servers it loaded
//! or called is compared with that recorded set.

use std::path::PathBuf;

use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{AgentBudget, AutomaticHold, Record, TurnOutcome};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::EnvVar;
use dudes_providers::mcp::{self, DeclaredServers, LoadedServers};
use dudes_providers::stream::SessionObservation;
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_supervisor::{Command, Input};
use dudes_types::{AgentId, BreakerSignal, McpServerName, Provider, Usd};

/// Server definitions as the daemon's parser builds them, from `(name, JSON table)` pairs.
fn declared(servers: &[(&str, &str)]) -> DeclaredServers {
    servers
        .iter()
        .map(|(name, table)| {
            mcp::declare(
                name,
                serde_json::from_str(table).expect("a declaration table"),
            )
            .expect("a valid declaration")
        })
        .collect()
}

fn loaded(servers: &[(&str, &str)], secrets: &[EnvVar]) -> LoadedServers {
    LoadedServers::new(&declared(servers), secrets, |command| {
        Some(format!("/opt/bin/{command}"))
    })
}

fn names(names: &[&str]) -> Vec<McpServerName> {
    names
        .iter()
        .map(|name| McpServerName::parse(name).expect("a server name"))
        .collect()
}

fn harness(mcp: LoadedServers, adapter: fn() -> Box<dyn Adapter>) -> Harness {
    Harness::with_config(
        dudes_supervisor::Config {
            mcp,
            ..Harness::config()
        },
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        adapter,
    )
}

fn claude() -> Box<dyn Adapter> {
    Box::new(ClaudeAdapter::default())
}

fn codex() -> Box<dyn Adapter> {
    Box::new(CodexAdapter::default())
}

fn pi() -> Box<dyn Adapter> {
    Box::new(dudes_providers::pi::PiAdapter::default())
}

fn hire(harness: &mut Harness, provider: Provider, trusted: bool, servers: &[&str]) -> AgentId {
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider,
        cwd: PathBuf::from("/repo"),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted,
        worktree: None,
        mcp_servers: names(servers),
    }));
    *harness
        .supervisor
        .world()
        .agents
        .keys()
        .next_back()
        .expect("the hire was journaled")
}

fn held_reasons(harness: &Harness) -> Vec<String> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::AgentHeld {
                cause: AutomaticHold::LaunchRefused { reason },
                ..
            } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn spawned(harness: &Harness) -> usize {
    harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Spawn { .. }))
        .count()
}

const MEMORY: (&str, &str) = (
    "memory",
    r#"{"url":"https://memory.example.com/mcp","header_env":{"Authorization":"MEMORY_AUTH"}}"#,
);

/// A launch admits the recorded servers again: a variable the daemon lacked at start holds the
/// agent before any turn is issued, with a reason naming the server and the variable, and nothing
/// spawns. The control is the same hire on a run that has the variable. Mutant: compose the launch
/// from the record without admitting it, so the agent runs with a server that cannot connect.
#[test]
fn a_launch_whose_server_lacks_its_variable_holds_the_agent_before_issuing() {
    let mut lacking = harness(loaded(&[MEMORY], &[]), claude);
    let agent = hire(&mut lacking, Provider::Claude, false, &["memory"]);
    lacking.set_goal(agent, &["npm", "test"]);
    assert_eq!(
        held_reasons(&lacking),
        [
            "declared MCP servers refused: MCP server `memory` needs `MEMORY_AUTH`, which was unset \
          or empty in the daemon's environment when it started; set it and restart the daemon"
        ]
    );
    assert!(
        lacking
            .records()
            .all(|record| !matches!(record, Record::TurnIssued { .. })),
        "the hold comes before any turn is issued"
    );
    assert_eq!(spawned(&lacking), 0);

    let mut control = harness(
        loaded(&[MEMORY], &[EnvVar::new("MEMORY_AUTH", "planted")]),
        claude,
    );
    let agent = hire(&mut control, Provider::Claude, false, &["memory"]);
    control.set_goal(agent, &["npm", "test"]);
    assert!(held_reasons(&control).is_empty());
    assert_eq!(spawned(&control), 1);
}

/// An operator who redefines a recorded http server as a program server and restarts cannot start
/// that program in an untrusted repository: the re-issued turn ends refused and the agent is held.
/// Mutant: check trust at the hire only.
#[test]
fn a_reissued_turn_refuses_a_server_redefined_as_a_program() {
    assert_reissued_refused(
        Provider::Claude,
        claude,
        "declared MCP servers refused: MCP server `notes`: a declared program server \
         starts in this repository's directory, where launchers such as npx read \
         configuration; trust the repository or declare an http server",
    );
    assert_reissued_refused(
        Provider::Pi,
        pi,
        "declared MCP servers refused: MCP server `notes` is a program server, and this engine \
         loads only http servers, because a program it starts would outlive the turn",
    );
}

fn assert_reissued_refused(provider: Provider, adapter: fn() -> Box<dyn Adapter>, reason: &str) {
    let mut first = harness(
        loaded(
            &[("notes", r#"{"url":"https://notes.example.com/mcp"}"#)],
            &[],
        ),
        adapter,
    );
    first.launch = false;
    let agent = hire(&mut first, provider, false, &["notes"]);
    first.set_goal(agent, &["npm", "test"]);
    assert!(
        first
            .records()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "the first run issued the turn and never spawned it"
    );

    let mut second = harness(
        loaded(&[("notes", r#"{"command":"notes-mcp"}"#)], &[]),
        adapter,
    );
    second.journal.clone_from(&first.journal);
    let recovery_from = second.trace.len();
    second.restart();
    assert!(
        second.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    second.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        second.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = second
        .supervisor
        .recovery_opportunity(agent)
        .unwrap()
        .expected;
    let handled = second.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(second.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let ended: Vec<&TurnOutcome> = second
        .records()
        .filter_map(|record| match record {
            Record::TurnEnded {
                outcome: Some(outcome),
                ..
            } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        [&TurnOutcome::LaunchRefused {
            reason: reason.to_owned()
        }]
    );
    assert_eq!(spawned(&second), 0, "nothing spawns");
    assert!(
        second.supervisor.world().agents[&agent]
            .automatic_hold
            .is_some(),
        "the refused re-issue holds the agent"
    );
}

fn beats(harness: &Harness) -> Vec<Vec<BreakerSignal>> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::BreakerBeat { signals, .. } => Some(signals.clone()),
            _ => None,
        })
        .collect()
}

fn observe(harness: &mut Harness, agent: AgentId, observation: SessionObservation) {
    let turn = harness.open_turn(agent).expect("a turn is running");
    harness.drive(harness.session_observation(turn, observation));
}

fn servers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

const ROGUE: (&str, &str) = ("rogue", r#"{"url":"https://rogue.example.com/mcp"}"#);
const PLAIN: (&str, &str) = ("a", r#"{"url":"https://a.example.com/mcp"}"#);

/// Claude's init against the set `{a}`: `{a}` raises nothing, `{a, b}` raises one leak, a set
/// missing `a` only logs, and the server set is per agent. Mutants: treat any server as a leak, as
/// before servers could be declared; or compare against every declared server instead of the
/// agent's own.
#[test]
fn a_claude_init_is_compared_with_the_agents_recorded_servers() {
    let declared = loaded(&[PLAIN, ROGUE], &[]);
    let mut declared_only = harness(declared.clone(), claude);
    let agent = hire(&mut declared_only, Provider::Claude, false, &["a"]);
    declared_only.set_goal(agent, &["npm", "test"]);
    observe(
        &mut declared_only,
        agent,
        SessionObservation {
            servers: servers(&["a"]),
            connected: Some(servers(&["a"])),
            ..SessionObservation::default()
        },
    );
    assert!(
        beats(&declared_only).is_empty(),
        "a declared server is no leak"
    );
    observe(
        &mut declared_only,
        agent,
        SessionObservation {
            servers: Vec::new(),
            connected: Some(Vec::new()),
            ..SessionObservation::default()
        },
    );
    assert!(
        beats(&declared_only).is_empty(),
        "a declared server that did not load narrows the turn"
    );

    let mut extra = harness(declared.clone(), claude);
    let agent = hire(&mut extra, Provider::Claude, false, &["a"]);
    extra.set_goal(agent, &["npm", "test"]);
    observe(
        &mut extra,
        agent,
        SessionObservation {
            servers: servers(&["a", "rogue"]),
            connected: Some(servers(&["a", "rogue"])),
            ..SessionObservation::default()
        },
    );
    assert_eq!(beats(&extra), [vec![BreakerSignal::IsolationLeak]]);

    let mut other_agent = harness(declared, claude);
    let agent = hire(&mut other_agent, Provider::Claude, false, &[]);
    other_agent.set_goal(agent, &["npm", "test"]);
    observe(
        &mut other_agent,
        agent,
        SessionObservation {
            servers: servers(&["a"]),
            connected: Some(servers(&["a"])),
            ..SessionObservation::default()
        },
    );
    assert_eq!(
        beats(&other_agent),
        [vec![BreakerSignal::IsolationLeak]],
        "a server declared for the daemon but not for this agent is a leak"
    );
}

/// Codex reports servers one at a time, from a tool call or a startup report: a server outside
/// the agent's set raises one leak, and one inside it nothing.
#[test]
fn a_codex_report_is_compared_with_the_agents_recorded_servers() {
    for (reported, leaks) in [("a", 0), ("rogue", 1), ("codex_apps", 1)] {
        let mut codex_floor = harness(loaded(&[PLAIN, ROGUE], &[]), codex);
        let agent = hire(&mut codex_floor, Provider::Codex, false, &["a"]);
        codex_floor.set_goal(agent, &["npm", "test"]);
        observe(
            &mut codex_floor,
            agent,
            SessionObservation {
                servers: servers(&[reported]),
                ..SessionObservation::default()
            },
        );
        assert_eq!(beats(&codex_floor).len(), leaks, "{reported}");
    }
}

#[test]
fn pi_completed_calls_are_compared_once_with_the_recorded_set() {
    use dudes_providers::stream::TurnReader;
    let pi = || Box::new(dudes_providers::pi::PiAdapter::default()) as Box<dyn Adapter>;
    for (given, tool, args, failed, leaks) in [
        (true, "mcp__b__x", serde_json::json!({}), false, 1),
        (true, "mcp__a__x", serde_json::json!({}), false, 0),
        (false, "mcp__a__x", serde_json::json!({}), false, 1),
        (true, "mcp__b__x", serde_json::json!({}), true, 0),
        (
            true,
            "read_mcp_resource",
            serde_json::json!({"server":"b"}),
            false,
            1,
        ),
        (true, "read", serde_json::json!({}), false, 0),
    ] {
        let mut run = harness(loaded(&[PLAIN], &[]), pi);
        let agent = hire(
            &mut run,
            Provider::Pi,
            false,
            if given { &["a"] } else { &[] },
        );
        run.set_goal(agent, &["npm", "test"]);
        let turn = run.open_turn(agent).unwrap();
        let mut reader: Box<dyn TurnReader> =
            Box::new(dudes_providers::pi::stream::TurnStream::new());
        reader.line(r#"{"type":"session","id":"session"}"#);
        for id in ["one", "two"] {
            reader.line(&serde_json::json!({"type":"tool_execution_start","toolCallId":id,"toolName":tool,"args":args}).to_string());
            reader.line(&serde_json::json!({"type":"tool_execution_end","toolCallId":id,"toolName":tool,"isError":failed}).to_string());
        }
        let end = reader.end(dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        });
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        if let Some(observation) = end.final_observation {
            run.drive(run.session_observation(turn, observation));
        }
        assert_eq!(
            beats(&run),
            if leaks == 1 {
                vec![vec![BreakerSignal::IsolationLeak]]
            } else {
                Vec::new()
            },
            "{given} {tool} {failed}"
        );
        assert!(!run.records().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: dudes_proto::decision::LoopDecision::Done,
                ..
            }
        )));
    }
}

/// A hire with no servers writes no field, so every journal written before the field has the same
/// shape and replays to no servers; one written with it replays to the same set after a restart.
/// Mutant: fold the field into the row on the live path only.
#[test]
fn the_recorded_set_replays() {
    let mut run = harness(loaded(&[PLAIN, ROGUE], &[]), claude);
    let none = hire(&mut run, Provider::Claude, false, &[]);
    let some = hire(&mut run, Provider::Claude, false, &["a", "rogue"]);
    let written: Vec<serde_json::Value> = run
        .records()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .map(|record| serde_json::to_value(record).expect("a record serialises"))
        .collect();
    assert_eq!(written[0].get("mcp_servers"), None);
    assert_eq!(written[1]["mcp_servers"], serde_json::json!(["a", "rogue"]));
    run.restart();
    let world = run.supervisor.world();
    assert!(world.agents[&none].mcp.desired.is_empty());
    assert_eq!(world.agents[&some].mcp.desired, names(&["a", "rogue"]));
}

#[test]
fn mcp_policy_retains_the_original_isolation_and_hire_revision() {
    let mut run = harness(loaded(&[], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &[]);
    let hired = run
        .records()
        .find(|record| matches!(record, Record::AgentHired { .. }))
        .unwrap();
    let mut value = serde_json::to_value(hired).unwrap();
    value["isolation"] = serde_json::json!("bare");
    let lsn = dudes_types::Lsn::from_parts(1, 100).unwrap();
    let mut world = dudes_supervisor::world::World::default();
    world.apply(
        lsn,
        &dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(0),
            record: serde_json::from_value(value).unwrap(),
        },
    );
    let policy = &world.agents[&agent].mcp;
    assert_eq!(policy.isolation, Some(dudes_types::IsolationMode::Bare));
    assert_eq!(policy.hire_lsn, lsn);
    assert_eq!(policy.revision, lsn);
    assert_eq!(policy.first_edit, None);
    assert!(!policy.browser_onboarded);
    assert!(policy.hire_servers.is_empty());
}

fn replace(harness: &mut Harness, agent: AgentId, selected: &[&str]) {
    harness.drive(Input::Command(Command::ReplaceMcp {
        agent,
        mcp_servers: names(selected),
    }));
}

fn grants(harness: &Harness) -> Vec<(dudes_types::Lsn, Vec<McpServerName>)> {
    harness
        .journal
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| match &entry.record {
            Record::McpLaunchGranted { mcp_servers, .. } => Some((
                dudes_types::Lsn(u64::try_from(position).unwrap() + 1),
                mcp_servers.clone(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_running_attempt_keeps_its_servers_after_replacement_with_or_without_a_grant() {
    for edited in [false, true] {
        let mut run = harness(loaded(&[PLAIN, ROGUE], &[]), claude);
        let agent = hire(
            &mut run,
            Provider::Claude,
            false,
            if edited { &[] } else { &["a"] },
        );
        if edited {
            replace(&mut run, agent, &["a"]);
        }
        run.set_goal(agent, &["/usr/bin/false"]);
        let turn = run.open_turn(agent).unwrap();
        let grant = run.supervisor.world().turns[&turn].mcp_grant;
        assert_eq!(grant.is_some(), edited);
        replace(&mut run, agent, &["rogue"]);
        observe(
            &mut run,
            agent,
            SessionObservation {
                servers: vec!["a".into()],
                ..SessionObservation::default()
            },
        );
        assert!(
            beats(&run).is_empty(),
            "removal does not revoke an old physical launch"
        );
        observe(
            &mut run,
            agent,
            SessionObservation {
                servers: vec!["rogue".into()],
                ..SessionObservation::default()
            },
        );
        assert_eq!(
            beats(&run),
            [vec![BreakerSignal::IsolationLeak]],
            "addition does not authorize the old child"
        );
        assert_eq!(run.supervisor.world().turns[&turn].mcp_grant, grant);
    }
}

#[test]
fn equal_replacement_does_not_activate_but_clear_gives_an_old_owed_turn_a_fresh_empty_grant() {
    let mut run = harness(loaded(&[PLAIN], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &["a"]);
    replace(&mut run, agent, &["a"]);
    assert_eq!(run.supervisor.world().agents[&agent].mcp.first_edit, None);
    assert!(grants(&run).is_empty());
    run.launch = false;
    run.set_goal(agent, &["/usr/bin/false"]);
    let turn = run.open_turn(agent).unwrap();
    assert!(grants(&run).is_empty());
    run.drive(Input::Command(Command::Halt { agent, text: None }));
    let count = spawned(&run);
    replace(&mut run, agent, &[]);
    assert_eq!(spawned(&run), count, "an edit does not resume or spawn");
    assert!(run.supervisor.world().agents[&agent].halted);
    run.launch = true;
    run.restart();
    assert_eq!(
        spawned(&run),
        count,
        "the halt survives recovery and the edit"
    );
    run.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(
        spawned(&run),
        count,
        "Resume does not authorize the owed goal issue"
    );
    let expected = run.supervisor.recovery_opportunity(agent).unwrap().expected;
    let handled = run.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(run.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(run.open_turn(agent), Some(turn));
    assert_eq!(grants(&run).len(), 1);
    assert!(grants(&run)[0].1.is_empty());
    assert_eq!(
        run.supervisor.world().turns[&turn].mcp_grant,
        Some(grants(&run)[0].0)
    );
    let grants_before = grants(&run).len();
    run.drive(Input::Started);
    assert_eq!(
        grants(&run).len(),
        grants_before,
        "no second physical launch in this run"
    );
}

#[test]
fn an_unacknowledged_grant_never_spawns_and_recovery_admits_a_fresh_attempt() {
    let mut run = harness(loaded(&[PLAIN], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &[]);
    replace(&mut run, agent, &["a"]);
    run.unfsynced
        .insert(dudes_proto::RecordKind::McpLaunchGranted);
    run.set_goal(agent, &["/usr/bin/false"]);
    let turn = run.open_turn(agent).unwrap();
    assert_eq!(spawned(&run), 0);
    assert!(
        grants(&run).is_empty(),
        "permission is not folded before fsync acknowledgement"
    );
    run.unfsynced
        .remove(&dudes_proto::RecordKind::McpLaunchGranted);
    let recovery_from = run.trace.len();
    run.restart();
    assert!(
        run.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    run.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        run.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = run.supervisor.recovery_opportunity(agent).unwrap().expected;
    let handled = run.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(run.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(spawned(&run), 1);
    assert_eq!(grants(&run).len(), 1);
    assert_eq!(
        run.supervisor.world().turns[&turn].mcp_grant,
        Some(grants(&run)[0].0)
    );
}

#[test]
fn observations_wait_for_the_matching_host_key_and_wrong_grants_are_not_relabelled() {
    let mut run = harness(loaded(&[PLAIN, ROGUE], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &[]);
    replace(&mut run, agent, &["a"]);
    run.launch = false;
    run.set_goal(agent, &["/usr/bin/false"]);
    let turn = run.open_turn(agent).unwrap();
    let grant = grants(&run)[0].0;
    let key = run.host.key_for(4242);
    run.drive(Input::SessionObserved {
        turn,
        key: key.clone(),
        mcp_grant: Some(grant),
        observation: SessionObservation {
            servers: vec!["rogue".into()],
            ..SessionObservation::default()
        },
    });
    assert!(
        beats(&run).is_empty(),
        "an early line cannot choose the physical key"
    );
    run.drive(Input::Spawned {
        turn,
        key: key.clone(),
        mcp_grant: Some(grant),
    });
    assert_eq!(beats(&run), [vec![BreakerSignal::IsolationLeak]]);
    let before = beats(&run).len();
    run.drive(Input::SessionObserved {
        turn,
        key: key.clone(),
        mcp_grant: None,
        observation: SessionObservation {
            servers: vec!["rogue".into()],
            ..SessionObservation::default()
        },
    });
    let mut recycled = key;
    recycled.start_time += 1;
    run.drive(Input::SessionObserved {
        turn,
        key: recycled,
        mcp_grant: Some(grant),
        observation: SessionObservation {
            servers: vec!["rogue".into()],
            ..SessionObservation::default()
        },
    });
    assert_eq!(beats(&run).len(), before);
}

#[test]
fn removing_an_unavailable_server_preserves_its_launch_hold_until_explicit_resume() {
    let mut run = harness(loaded(&[MEMORY], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &["memory"]);
    run.set_goal(agent, &["/usr/bin/false"]);
    let hold = run.supervisor.world().agents[&agent].automatic_hold.clone();
    assert!(hold.is_some());
    replace(&mut run, agent, &[]);
    assert_eq!(run.supervisor.world().agents[&agent].automatic_hold, hold);
    assert_eq!(spawned(&run), 0);
    assert!(grants(&run).is_empty());
    run.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(spawned(&run), 1);
    assert!(grants(&run)[0].1.is_empty());
}

#[test]
fn a_launch_parked_before_its_grant_reselects_the_latest_desired_material() {
    let b = ("b", r#"{"url":"http://127.0.0.1:4502/mcp"}"#);
    let mut run = harness(loaded(&[PLAIN, b], &[]), claude);
    let agent = hire(&mut run, Provider::Claude, false, &[]);
    replace(&mut run, agent, &["a"]);
    run.unfsynced.insert(dudes_proto::RecordKind::TurnIssued);
    run.set_goal(agent, &["/usr/bin/false"]);
    let (issued_token, turn) = run
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Persist { token, entry } => match entry.record {
                Record::TurnIssued { turn, .. } => Some((*token, turn)),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(spawned(&run), 0);
    assert!(grants(&run).is_empty());
    replace(&mut run, agent, &["b"]);
    let issued_lsn = dudes_types::Lsn(u64::try_from(run.journal.len()).unwrap() + 1);
    let granted = run.step(run.persisted(issued_token, issued_lsn));
    let grant_token = granted
        .iter()
        .find_map(|seen| match seen {
            Seen::Persist { token, entry } => match &entry.record {
                Record::McpLaunchGranted {
                    turn: granted,
                    mcp_servers,
                    policy_lsn,
                    ..
                } if *granted == turn => {
                    assert_eq!(*mcp_servers, names(&["b"]));
                    assert_eq!(
                        *policy_lsn,
                        run.supervisor.world().agents[&agent].mcp.revision
                    );
                    Some(*token)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the latest set is granted before dispatch");
    let grant_lsn = dudes_types::Lsn(issued_lsn.0 + 1);
    let mut ready = run.step(run.persisted(grant_token, grant_lsn));
    if let Some(token) = ready.iter().find_map(|seen| match seen {
        Seen::Persist { token, entry }
            if matches!(entry.record, Record::HookTokenMinted { .. }) =>
        {
            Some(*token)
        }
        _ => None,
    }) {
        ready = run.step(run.persisted(token, dudes_types::Lsn(grant_lsn.0 + 1)));
    }
    let spec = ready
        .iter()
        .find_map(|seen| match seen {
            Seen::Spawn {
                turn: spawned,
                spec,
                mcp_grant,
                ..
            } if *spawned == turn => {
                assert_eq!(*mcp_grant, Some(grant_lsn));
                Some(spec)
            }
            _ => None,
        })
        .expect("one prepared launch after the acknowledgements");
    let config = spec
        .argv
        .windows(2)
        .find(|words| words[0] == "--mcp-config")
        .unwrap()[1]
        .to_str()
        .unwrap();
    let config: serde_json::Value = serde_json::from_str(config).unwrap();
    assert_eq!(
        config["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["b"]
    );
}
