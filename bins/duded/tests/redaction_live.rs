//! A secret in the environment a child starts from is scrubbed from what agents say, from the
//! moment the child launches.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_providers::launch::EnvVar;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

#[test]
fn an_ambient_secret_and_the_operator_credential_are_scrubbed_once_a_child_runs() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.daemon_env.push(EnvVar::new(
        "PLANTED_API_TOKEN",
        "ghp_Zq81KdPvR4sWbN7hYcA3eJ",
    ));
    settings
        .daemon_env
        .push(EnvVar::new("NODE_ENV", "production"));
    let host = SharedHost::new();
    let seams = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let redactor = seams.children.redactor();
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);

    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    assert_eq!(
        redactor.scrub(&credential),
        "(redacted)",
        "the operator credential is a secret from boot"
    );
    assert_eq!(
        redactor.scrub("ghp_Zq81KdPvR4sWbN7hYcA3eJ"),
        "ghp_Zq81KdPvR4sWbN7hYcA3eJ",
        "an environment value is held once a child launches with it, not before"
    );

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_turn();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.spawned().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no child launched");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        redactor.scrub("x ghp_Zq81KdPvR4sWbN7hYcA3eJ y NODE_ENV=production"),
        "x (redacted) y NODE_ENV=production"
    );
    running.stop(ExitCause::Ended);
}
