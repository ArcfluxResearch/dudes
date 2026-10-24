//! A secret in the environment a child starts from is scrubbed from what agents say, from the
//! moment the child launches.
#![cfg(unix)]

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use duded::output::OutputState;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::{Record, TurnOutcome};
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::Redactor;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};
use dudes_types::{AgentId, SessionId, TurnId};

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

const PLANTED: &str = "ghp_Zq81KdPvR4sWbN7hYcA3eJ";
const SHORT: &str = "1234567";

/// A host that records what the secret set does to a probe text at the moment of each launch.
struct ProbingHost<H> {
    inner: H,
    redactor: Arc<OnceLock<Redactor>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl<H: ProcessHost> ProcessHost for ProbingHost<H> {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let probe = format!("x {PLANTED} y NODE_ENV=production k={SHORT}");
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(
                self.redactor
                    .get()
                    .expect("wired")
                    .scrub(&probe)
                    .into_owned(),
            );
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown()
    }
}

#[test]
fn the_environment_is_held_when_the_launch_is_attempted_and_released_when_it_never_happened() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings
        .daemon_env
        .push(EnvVar::new("PLANTED_API_TOKEN", PLANTED));
    settings
        .daemon_env
        .push(EnvVar::new("NODE_ENV", "production"));
    settings
        .daemon_env
        .push(EnvVar::new("SHORT_API_KEY", SHORT));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let wired = Arc::new(OnceLock::new());
    let seams = seams(
        &settings,
        ProbingHost {
            inner: common::ConfirmedFailureHost::new(),
            redactor: Arc::clone(&wired),
            seen: Arc::clone(&seen),
        },
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let redactor = seams.children.redactor();
    wired.set(redactor.clone()).expect("wired once");
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("NotLaunched", |record| match record {
        Record::TurnEnded {
            outcome: Some(TurnOutcome::NotLaunched { .. }),
            ..
        } => Some(()),
        _ => None,
    });
    assert_eq!(
        seen.lock().unwrap_or_else(PoisonError::into_inner)[0],
        format!("x (redacted) y NODE_ENV=production k={SHORT}"),
        "held at the launch; an ordinary value and the 7-byte residual stay visible"
    );
    assert_eq!(
        redactor.scrub(PLANTED),
        PLANTED,
        "a launch the host confirmed never happened holds nothing"
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn the_operator_credential_and_an_mcp_secret_never_reach_the_ring() {
    const MCP_SECRET: &str = "planted-mcp-header-value";
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings
        .daemon_env
        .push(EnvVar::new("MEMORY_AUTH", MCP_SECRET));
    let options = duded::settings::RunOptions::default().with_mcp_servers(
        dudes_providers::mcp::parse_servers(
            toml::from_str(
                r#"
[memory]
url = "https://memory.example.com/mcp"
header_env = { "Authorization" = "MEMORY_AUTH" }
"#,
            )
            .expect("a TOML table"),
        )
        .expect("a valid declaration"),
    );
    let seams = seams(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let output = seams.children.output();
    let daemon = duded::boot::boot_with_options(&settings, seams, options).expect("boot");
    let mut running = common::start(daemon);
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");

    let id = |n: u128| ulid::Ulid::from(n);
    let (agent, turn) = (AgentId::from(id(1)), TurnId::from(id(2)));
    let key = AdoptionKey {
        pid: 42,
        start_time: 10,
        boot: Some("boot-a".into()),
    };
    output.replace(agent, turn);
    output.register(agent, turn, &key);
    output.confirm(agent, turn, &SessionId::new("s"));
    output.capture(
        &key,
        "s",
        &format!("cred {credential} and mcp {MCP_SECRET} done"),
    );
    output.watch_finished(&key);
    let OutputState::Ready { chunks, .. } = output.snapshot(agent).state else {
        panic!("the ring was not ready");
    };
    let text: String = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
    assert_eq!(text, "cred (redacted) and mcp (redacted) done");
    running.stop(ExitCause::Ended);
}
