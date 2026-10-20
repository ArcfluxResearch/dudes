#![allow(missing_docs)]
#![cfg(all(unix, debug_assertions))]

use std::sync::Arc;

use dudes::exit;
use dudes_proto::Record;
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::goal::GoalSetResult;
use dudes_types::{AgentId, GoalId};
use serde_json::Value;

mod common;
use common::{Cli, Fixture, OpenRunner, Running, SharedHost, goal_args, hire_args, start};

struct Stopped {
    fixture: Fixture,
    daemon: Running,
    host: SharedHost,
    cli: Cli,
    agent: String,
    lsn: u64,
}

/// The agent whose goal starts the failure: `agent` itself, or, when it is retired, a live agent
/// hired beside it, because a retired agent's goal is refused before any append.
fn initiator(cli: &Cli, fixture: &Fixture, agent: &str, standing: Option<&str>) -> String {
    if standing != Some("retired") {
        return agent.to_owned();
    }
    let bystander = cli
        .clone()
        .args(hire_args(fixture.state.path()))
        .arg("--json")
        .run();
    assert_eq!(bystander.code(), exit::SUCCESS, "{}", bystander.output());
    bystander.json::<HireResult>().agent.to_string()
}

impl Stopped {
    fn new() -> Self {
        Self::with_background_failure(false)
    }

    fn with_background_failure(background: bool) -> Self {
        Self::with_state(background, None)
    }

    fn with_state(background: bool, standing: Option<&str>) -> Self {
        Self::with_fault(background, standing, false)
    }

    fn with_fault(background: bool, standing: Option<&str>, checkpoint: bool) -> Self {
        Self::with_pre_failure(background, standing, checkpoint, |_, _, _| {})
    }

    fn with_pre_failure(
        background: bool,
        standing: Option<&str>,
        checkpoint: bool,
        before: impl FnOnce(&Fixture, &Cli, &str),
    ) -> Self {
        let fixture = Fixture::new();
        let mut settings = fixture.settings();
        if checkpoint {
            settings.journal.checkpoint_every = 1;
        }
        let host = SharedHost::new();
        let daemon = start(&settings, host.clone(), Arc::new(OpenRunner::default()));
        let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
        let hired = cli
            .clone()
            .args(hire_args(fixture.state.path()))
            .arg("--json")
            .run();
        assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
        let agent = hired.json::<HireResult>().agent.to_string();
        if let Some(state) = standing {
            let agent = agent.parse().unwrap();
            daemon
                .handle
                .ask(duded::ports::Ask::Steer {
                    agent,
                    text: "pending correction".to_owned(),
                    provenance: dudes_proto::PeerProvenance::Unresolvable,
                })
                .expect("pending correction is durable before standing control");
            if state != "steered" {
                let ask = match state {
                    "paused" => duded::ports::Ask::Pause(agent),
                    "halted" => duded::ports::Ask::Halt(agent),
                    "retired" => {
                        duded::ports::Ask::Retire(Box::new(dudes_proto::rpc::agent::RetireParams {
                            agent,
                            reason: "operator retirement".to_owned(),
                        }))
                    }
                    _ => panic!("unknown standing state"),
                };
                daemon
                    .handle
                    .ask(ask)
                    .expect("standing state is durable before failure");
            }
        }
        let initiator = initiator(&cli, &fixture, &agent, standing);
        before(&fixture, &cli, &agent);
        if checkpoint {
            daemon.append_failure.short_write_after(1, 5);
        } else {
            daemon.append_failure.short_write_next(5);
        }
        if background {
            daemon.handle.input(dudes_supervisor::Input::Command(
                dudes_supervisor::Command::Pause {
                    agent: agent.parse().unwrap(),
                    browser: None,
                },
            ));
            daemon
                .handle
                .doctor()
                .expect("diagnostics survive background append failure");
        } else {
            let failed = cli
                .clone()
                .arg("--json")
                .args(goal_args(&initiator, "first goal"))
                .run();
            assert_ne!(failed.code(), exit::SUCCESS);
            assert_eq!(
                failed.json_error().code,
                -32603,
                "the initiating failure retains its original error"
            );
        }
        let mut files: Vec<_> = std::fs::read_dir(fixture.state.path().join("journal"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        let file = files.last().unwrap();
        let segment = file
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let lsn = dudes_types::Lsn::from_parts(segment, std::fs::metadata(file).unwrap().len() - 5)
            .unwrap()
            .0;
        Self {
            fixture,
            daemon,
            host,
            cli,
            agent,
            lsn,
        }
    }

    fn expected(&self) -> String {
        format!(
            "journal stopped accepting writes at segment {} · byte {}; likely a full disk or an I/O error; free space and restart the daemon",
            dudes_types::Lsn(self.lsn).segment(),
            dudes_types::Lsn(self.lsn).offset()
        )
    }
}

fn await_goal_spawn(fixture: &Fixture, agent: AgentId, goal: GoalId) {
    let turn = fixture.await_record("issued turn for the repaired goal", |record| match record {
        Record::TurnIssued {
            agent: owner,
            goal: issued_goal,
            turn,
            ..
        } if *owner == agent && *issued_goal == goal => Some(*turn),
        _ => None,
    });
    fixture.await_record("spawn of the repaired goal's exact turn", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, turn: spawned, .. }
            if *owner == agent && *spawned == turn)
        .then_some(())
    });
}

#[test]
fn status_names_stopped_journal_and_recovery() {
    let stopped = Stopped::new();
    let result = stopped.cli.clone().arg("status").run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert!(
        result.stdout.lines().any(|line| line == stopped.expected()),
        "{}",
        result.output()
    );
    let json = stopped
        .cli
        .clone()
        .args(["status", "--json"])
        .run()
        .json::<Value>();
    assert_eq!(json["journal_stopped"]["lsn"], stopped.lsn);
}

#[test]
fn new_turn_is_refused_without_an_append_or_spawn() {
    let stopped = Stopped::new();
    stopped.daemon.handle.input(dudes_supervisor::Input::Tick);
    let journal = stopped.fixture.state.path().join("journal/000001.log");
    let before = std::fs::read(&journal).unwrap();
    let result = stopped
        .cli
        .clone()
        .arg("--json")
        .args(goal_args(&stopped.agent, "new goal"))
        .run();
    let error = result.json_error();
    assert_eq!(error.code, -32006, "{}", result.output());
    assert_eq!(error.data.unwrap()["lsn"], stopped.lsn);
    assert_eq!(error.message, stopped.expected());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    assert_eq!(stopped.host.children(), 0);
}

#[test]
fn restart_repairs_torn_tail_and_resumes_writes() {
    let mut stopped = Stopped::new();
    stopped.daemon.stop();
    let settings = stopped.fixture.settings();
    let mut restarted = start(
        &settings,
        stopped.host.clone(),
        Arc::new(OpenRunner::default()),
    );
    let status = stopped.cli.clone().args(["status", "--json"]).run();
    assert_eq!(status.code(), exit::SUCCESS, "{}", status.output());
    assert!(status.json::<Value>().get("journal_stopped").is_none());
    let result = stopped
        .cli
        .clone()
        .arg("--json")
        .args(goal_args(&stopped.agent, "repaired goal"))
        .run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    await_goal_spawn(
        &stopped.fixture,
        stopped.agent.parse().unwrap(),
        result.json::<GoalSetResult>().goal,
    );
    assert_eq!(stopped.host.children(), 1);
    assert!(stopped.fixture.records().iter().any(|record| matches!(record, dudes_proto::Record::GoalSet { goal, .. } if goal.text == "repaired goal")));
    restarted.stop();
}

#[test]
fn background_failure_keeps_diagnostics_reachable_without_retry() {
    let stopped = Stopped::with_background_failure(true);
    let journal = stopped.fixture.state.path().join("journal/000001.log");
    let before = std::fs::read(&journal).unwrap();
    stopped.daemon.handle.input(dudes_supervisor::Input::Tick);
    let result = stopped.cli.clone().arg("status").run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert!(
        result.stdout.lines().any(|line| line == stopped.expected()),
        "{}",
        result.output()
    );
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    assert_eq!(stopped.host.children(), 0);
}

fn hook_response(stopped: &Stopped, event: &str) -> dudes_proto::rpc::Response {
    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_rpc::auth::{PeerClassifier, PeerProvenance};
    use dudes_rpc::testing::FakeMintedTokens;
    let agent = stopped.agent.parse().unwrap();
    let mut dispatcher = dudes_rpc::Dispatcher::new(PeerClassifier::new(
        OperatorCredential::new("C".repeat(43)).unwrap(),
        Arc::new(FakeMintedTokens::minted(&[(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            agent,
        )])),
    ));
    duded::hooks::register(
        &mut dispatcher,
        duded::hooks::Ingress::new(
            stopped.daemon.handle.clone(),
            Arc::new(common::TestClock::new()),
        ),
        std::num::NonZeroUsize::MIN,
    );
    let line = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "hook.event",
        "params": { "token": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "raw": {
            "hook_event_name": event, "session_id": "00000000-0000-4000-8000-000000000001",
            "tool_name": "Read", "tool_input": {}, "prompt": "test prompt"
        }}
    });
    dispatcher.handle_line(
        &line.to_string(),
        PeerProvenance::Agent(agent),
        &Arc::new(dudes_rpc::Outbound::sink()),
    )
}

fn assert_poisoned_journal_preserves_hook_controls(state: &str) {
    let stopped = Stopped::with_state(false, Some(state));
    for event in ["PreToolUse", "UserPromptSubmit"] {
        let response = hook_response(&stopped, event);
        assert!(response.error.is_none(), "{state} {event}: {response:?}");
        let reply: dudes_proto::VendorReply =
            serde_json::from_value(response.result.unwrap()).unwrap();
        let expected = if state == "paused" {
            dudes_proto::VendorReply {
                exit_code: 2,
                body: Some(if event == "PreToolUse" {
                    serde_json::json!({ "hookSpecificOutput": {
                        "hookEventName": "PreToolUse", "permissionDecision": "deny",
                        "permissionDecisionReason": "the loop is paused"
                    }})
                } else {
                    serde_json::json!({"decision": "block", "reason": "the loop is paused"})
                }),
            }
        } else {
            dudes_proto::VendorReply {
                exit_code: 0,
                body: Some(serde_json::json!({"continue": false})),
            }
        };
        assert_eq!(reply, expected, "{state} {event}");
    }
}

#[test]
fn poisoned_journal_preserves_paused_hook_controls() {
    assert_poisoned_journal_preserves_hook_controls("paused");
}

#[test]
fn poisoned_journal_preserves_halted_hook_controls() {
    assert_poisoned_journal_preserves_hook_controls("halted");
}

#[test]
fn poisoned_journal_preserves_retired_hook_controls() {
    assert_poisoned_journal_preserves_hook_controls("retired");
}

#[test]
fn retired_agent_refusal_precedes_stopped_journal_diagnostics() {
    let stopped = Stopped::with_state(false, Some("retired"));
    let error = stopped
        .daemon
        .handle
        .ask(duded::ports::Ask::Ask {
            agent: stopped.agent.parse().unwrap(),
            text: "should remain refused".to_owned(),
        })
        .unwrap_err();
    assert_eq!(error.code, -32004);
    assert_eq!(error.data.unwrap()["gate"], "token");
}

#[test]
fn poisoned_hook_cannot_consume_pending_steer() {
    let stopped = Stopped::with_state(false, Some("steered"));
    let journal = stopped.fixture.state.path().join("journal/000001.log");
    let before = std::fs::read(&journal).unwrap();
    let response = hook_response(&stopped, "UserPromptSubmit");
    let error = response
        .error
        .expect("steer delivery requires its consumption record");
    assert_eq!(error.code, -32006);
    assert_eq!(error.data.unwrap()["lsn"], stopped.lsn);
    assert_eq!(std::fs::read(&journal).unwrap(), before);
}

#[test]
fn checkpoint_poison_preserves_durable_frame_and_prevents_spawn() {
    let stopped = Stopped::with_fault(false, None, true);
    let result = stopped.cli.clone().args(["status", "--json"]).run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert_eq!(
        result.json::<Value>()["journal_stopped"]["lsn"],
        stopped.lsn
    );
    assert_eq!(stopped.host.children(), 0);
    let last_applied = result.json::<Value>()["ping"]["lsn"].as_u64().unwrap();
    assert!(
        last_applied < stopped.lsn,
        "a failed checkpoint is not a durable cursor"
    );
    let mut stopped = stopped;
    stopped.daemon.stop();
    let settings = stopped.fixture.settings();
    let mut restarted = start(
        &settings,
        stopped.host.clone(),
        Arc::new(OpenRunner::default()),
    );
    assert!(stopped.fixture.records().iter().any(|record| matches!(record, dudes_proto::Record::GoalSet { goal, .. } if goal.text == "first goal")));
    let agent = stopped.agent.parse().unwrap();
    let goal = stopped
        .fixture
        .await_record("durable first goal", |record| match record {
            Record::GoalSet {
                agent: owner, goal, ..
            } if *owner == agent && goal.text == "first goal" => Some(goal.id),
            _ => None,
        });
    await_goal_spawn(&stopped.fixture, agent, goal);
    assert_eq!(
        stopped.host.children(),
        1,
        "restart resumes the durable goal exactly once"
    );
    restarted.stop();
}

#[test]
fn doctor_and_status_preserve_both_diagnostics_and_escape_labels() {
    let label = "marker\u{1b}]52;clipboard\n\u{85}\u{202e}forged";
    let stopped = Stopped::with_pre_failure(false, None, false, |fixture, cli, agent| {
        let done = fixture
            .state
            .path()
            .join("agents")
            .join(agent)
            .join("inbox/.done");
        std::fs::create_dir_all(&done).unwrap();
        std::fs::write(done.join(label), "unmatched content").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let result = cli.clone().args(["status", "--json"]).run();
            let value = result.json::<Value>();
            assert!(
                value["journal_stopped"].is_null(),
                "writer already stopped before injection"
            );
            if value["mailbox"]["diagnostics"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["filename"] == label))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mailbox observation did not converge: available={}, diagnostics={}, agents={}, directory_entries={}",
                value["mailbox"]["available"],
                value["mailbox"]["diagnostics"]
                    .as_array()
                    .map_or(0, Vec::len),
                value["agents"].as_array().map_or(0, Vec::len),
                std::fs::read_dir(&done).unwrap().count()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    {
        let command = "status";
        let human = stopped.cli.clone().arg(command).run();
        assert_eq!(human.code(), exit::SUCCESS);
        assert!(human.stdout.contains(&stopped.expected()));
        assert!(
            human
                .stdout
                .contains("marker\\u{1b}]52;clipboard\\u{a}\\u{85}\\u{202e}forged")
        );
        let machine = stopped.cli.clone().args([command, "--json"]).run();
        assert_eq!(machine.code(), exit::SUCCESS);
        let json = machine.json::<Value>();
        assert_eq!(json["journal_stopped"]["lsn"], stopped.lsn);
        assert!(
            json["mailbox"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["filename"] == label)
        );
        for control in ['\u{1b}', '\n', '\u{85}', '\u{202e}'] {
            assert!(!machine.stdout.trim_end_matches('\n').contains(control));
        }
    }
}
