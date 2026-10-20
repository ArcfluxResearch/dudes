//! Canonical-folder admission at the daemon's operator hire boundary.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ports::{Answered, Ask};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_host::RealHost;
use dudes_proto::Record;
use dudes_proto::rpc::ErrorCode;
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::EnvVar;
use dudes_providers::registry::Adapters;
use dudes_types::IsolationMode;

mod common;

use common::{Fixture, ScriptedRunner, TestClock, seams};

fn hire_count(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .count()
}

#[test]
fn configured_inherit_refuses_a_canonical_alias_of_a_live_owned_peers_folder() {
    let fixture = Fixture::new();
    let shared = fixture.state.path().join("shared");
    let alias = fixture.state.path().join("shared-alias");
    let separate = fixture.state.path().join("separate");
    let standin = fixture.state.path().join("stand-in");
    for folder in [&shared, &separate, &standin] {
        std::fs::create_dir(folder).expect("create fixture folder");
    }
    std::os::unix::fs::symlink(&shared, &alias).expect("canonical alias");
    let command = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/two-roles/bin/claude")
        .canonicalize()
        .expect("owned stand-in executable");
    let settings = duded::settings::Settings {
        daemon_env: vec![
            EnvVar::new("PATH", "/usr/bin:/bin"),
            EnvVar::new("HOME", fixture.state.path().as_os_str()),
            EnvVar::new("DUDES_EXAMPLE_WORK", standin.as_os_str()),
            EnvVar::new("DUDES_EXAMPLE_HOLD_SECONDS", "30"),
        ],
        ..fixture.settings()
    };
    let mut injected = seams(
        &settings,
        RealHost::with_grace(Duration::from_millis(300)),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    injected.adapters = Adapters::new([Box::new(ClaudeAdapter::new(ClaudeConfig {
        command: command.into_os_string(),
        isolation: IsolationMode::Inherit,
        ..ClaudeConfig::default()
    })) as Box<dyn Adapter>]);
    let daemon = boot(&settings, injected).expect("boot");
    let mut running = common::start(daemon);
    let peer = match running
        .handle
        .ask(Ask::Hire(Box::new(dudes_proto::rpc::agent::HireParams {
            name: "Live peer".to_owned(),
            trusted: true,
            ..common::hire_params(&shared)
        })))
        .expect("the first inherit hire is admitted")
    {
        Answered::Hired(result) => result.agent,
        other => panic!("unexpected hire answer {other:?}"),
    };
    common::set_goal(
        &running.handle,
        common::goal_params(peer, &["/usr/bin/true"]),
    );
    fixture.await_record("the real owned child receipt", |record| match record {
        Record::TurnSpawned { agent, .. } if *agent == peer => Some(()),
        _ => None,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !standin.join("started.1").is_file() {
        assert!(Instant::now() < deadline, "the owned child starts its hold");
        std::thread::sleep(Duration::from_millis(10));
    }

    for isolation in [
        None,
        Some(IsolationMode::Default),
        Some(IsolationMode::Inherit),
    ] {
        let before = hire_count(&fixture);
        let refusal = running
            .handle
            .ask(Ask::Hire(Box::new(dudes_proto::rpc::agent::HireParams {
                name: "Candidate".to_owned(),
                trusted: true,
                isolation,
                ..common::hire_params(&alias)
            })))
            .expect_err("configured inherit refuses a live peer in the canonical folder");
        assert_eq!(refusal.code, ErrorCode::Refused.code());
        assert!(refusal.message.contains("Live peer"), "{refusal:?}");
        assert!(refusal.message.contains(&peer.to_string()), "{refusal:?}");
        assert!(refusal.message.contains("worktree"), "{refusal:?}");
        assert_eq!(hire_count(&fixture), before, "a refusal appends no hire");
    }

    let answer = running
        .handle
        .ask(Ask::Hire(Box::new(dudes_proto::rpc::agent::HireParams {
            name: "Separate".to_owned(),
            trusted: true,
            ..common::hire_params(&separate)
        })))
        .expect("a distinct canonical folder admits inherit");
    let Answered::Hired(result) = answer else {
        panic!("unexpected distinct-folder answer {answer:?}");
    };
    assert_ne!(result.agent, peer);
    assert_eq!(hire_count(&fixture), 2);
    running.stop(ExitCause::ShutdownRpc);
}

fn launch_refusal_peers(
    fixture: &Fixture,
    host: &common::SharedHost,
) -> (common::Running, dudes_types::AgentId, dudes_types::AgentId) {
    let settings = fixture.settings();
    let seams = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&fixture.settings(), seams).expect("boot");
    let running = common::start(daemon);
    let peer = common::hire(&running.handle, fixture.state.path());
    let candidate = match running
        .handle
        .ask(Ask::Hire(Box::new(dudes_proto::rpc::agent::HireParams {
            name: "Candidate".to_owned(),
            trusted: true,
            isolation: Some(IsolationMode::Inherit),
            ..common::hire_params(fixture.state.path())
        })))
        .expect("unspawned peers do not refuse a hire")
    {
        Answered::Hired(result) => result.agent,
        other => panic!("unexpected hire {other:?}"),
    };
    (running, peer, candidate)
}

#[test]
fn launch_refusal_keeps_the_issued_task_until_the_exact_peer_boundary_is_absent() {
    let fixture = Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let (mut running, peer, candidate) = launch_refusal_peers(&fixture, &host);
    common::set_goal(
        &running.handle,
        common::goal_params(peer, &["/usr/bin/true"]),
    );
    let (peer_turn, pid, start) = fixture.await_record("peer child", |record| match record {
        Record::TurnSpawned {
            agent,
            turn,
            pid,
            start_time,
            ..
        } if *agent == peer => Some((*turn, *pid, *start_time)),
        _ => None,
    });
    running.handle.ask(Ask::Pause(peer)).unwrap();
    common::set_goal(
        &running.handle,
        common::goal_params(candidate, &["/usr/bin/true"]),
    );
    let (turn, task) = fixture.await_record("candidate issue", |record| match record {
        Record::TurnIssued {
            agent, turn, task, ..
        } if *agent == candidate => Some((*turn, task.clone())),
        _ => None,
    });
    let reason = fixture.await_record("durable named admission hold", |record| match record {
        Record::AgentHeld {
            agent,
            cause: dudes_proto::AutomaticHold::LaunchRefused { reason },
            ..
        } if *agent == candidate => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(
        reason,
        format!(
            "Claude inherit is refused while Hector ({peer}) is live in the same folder; use a worktree"
        )
    );
    assert_eq!(host.spawned().len(), 1);
    assert!(fixture.records().iter().all(|record| !matches!(record,
        Record::TurnEnded { turn: ended, .. } if *ended == turn)));
    fixture.await_record(
        "the refused attempt's explicit no-host-call proof",
        |record| {
            matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed { agent, turn: refused, .. },
            settlement: dudes_proto::InstructionSettlement::NoProcess {
                proof: dudes_proto::InstructionNoProcessProof::NoHostCall, ..
            }, ..
        } if *agent == candidate && *refused == turn)
            .then_some(())
        },
    );

    host.answer(pid, dudes_supervisor::ProbeAnswer::CouldNotAnswer);
    let before = fixture.entries().last().unwrap().0;
    running
        .handle
        .ask(Ask::Resume(candidate))
        .expect("resume answers with a durable uncertainty hold");
    let reason = next_refusal(&fixture, candidate, before);
    assert!(reason.contains("unresolved liveness") && !reason.contains("is live"));
    assert_eq!(host.spawned().len(), 1);

    host.answer(pid, dudes_supervisor::ProbeAnswer::StartValue(start + 1));
    let before = fixture.entries().last().unwrap().0;
    running
        .handle
        .ask(Ask::Resume(candidate))
        .expect("a recycled leader alone does not prove native boundary absence");
    let reason = next_refusal(&fixture, candidate, before);
    assert!(reason.contains("unresolved liveness") && !reason.contains("is live"));
    assert_eq!(host.spawned().len(), 1);
    common::end_turn_with_native(&fixture, &running.handle, &host, peer_turn);
    let key = host.peek(|host| host.key_for(pid));
    assert!(host.native_acknowledged(&key));
    running.handle.ask(Ask::Resume(candidate)).unwrap();
    assert_immutable_retry(&fixture, &host, candidate, turn, &task);
    running.stop(ExitCause::ShutdownRpc);
}

fn assert_immutable_retry(
    fixture: &Fixture,
    host: &common::SharedHost,
    candidate: dudes_types::AgentId,
    turn: dudes_types::TurnId,
    task: &str,
) {
    fixture.await_record("the same issued turn launches", |record| match record {
        Record::TurnSpawned {
            agent,
            turn: spawned,
            ..
        } if *agent == candidate && *spawned == turn => Some(()),
        _ => None,
    });
    let spawns = host.spawned();
    assert_eq!(spawns.len(), 2);
    assert_eq!(
        spawns[1].argv.last().and_then(|part| part.to_str()),
        Some(task)
    );
    assert!(
        spawns[1]
            .argv
            .iter()
            .any(|part| part == "--setting-sources"),
        "the recorded selector still does not override configured argv"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record,
        Record::TurnIssued { agent, .. } if *agent == candidate))
            .count(),
        1
    );
}

fn next_refusal(
    fixture: &Fixture,
    candidate: dudes_types::AgentId,
    after: dudes_types::Lsn,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reason) =
            fixture
                .entries()
                .iter()
                .find_map(|(lsn, entry)| match &entry.record {
                    Record::AgentHeld {
                        agent,
                        cause: dudes_proto::AutomaticHold::LaunchRefused { reason },
                        ..
                    } if *lsn > after && *agent == candidate => Some(reason.clone()),
                    _ => None,
                })
        {
            return reason;
        }
        assert!(
            Instant::now() < deadline,
            "the sampled retry must reach its actual hold"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
