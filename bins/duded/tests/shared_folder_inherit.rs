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
