//! Authenticated Floor reads during the daemon's host-shutdown boundary.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};
use duded::floor_browser::Page;
use duded::settings::FloorSettings;
use duded::startup::{TeardownStep, Witness};
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::system::ShutdownResult;
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_rpc::Client;
use dudes_supervisor::Clock as _;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};
use dudes_types::AgentId;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v11"
);
const GATE_BOUND: Duration = Duration::from_secs(60);

struct ShutdownGate {
    host: SharedHost,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl ProcessHost for ShutdownGate {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.host.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.host.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.host.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.host.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.entered.send(()).expect("shutdown entered");
        self.release
            .recv_timeout(GATE_BOUND)
            .expect("the test releases shutdown");
        self.host.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        self.host.write_hooks(install)
    }
}

struct Release(mpsc::Sender<()>);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct Browser(Page);

fn credential(settings: &duded::Settings) -> OperatorCredential {
    let value = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("scratch operator credential");
    OperatorCredential::new(value).expect("credential")
}

impl Browser {
    fn authenticate(settings: &duded::Settings, addr: SocketAddr, clock: &Arc<TestClock>) -> Self {
        let clock = Arc::clone(clock);
        Self(common::floor_session_on(settings, addr, move || {
            clock.now().unix_millis()
        }))
    }

    fn post(&self, path: &str, agent: AgentId) -> (u16, Vec<u8>) {
        let body = if path == "/floor/memory/page" {
            json!({"agent_id": agent.to_string(), "before": null})
        } else {
            json!({"agent_id": agent.to_string()})
        };
        let reply = self.0.post(path, &body);
        (reply.status(), reply.body)
    }
}

#[expect(
    clippy::print_stdout,
    reason = "retain the actual HTTP response and durable shutdown receipt as runtime proof"
)]
fn shutdown_read(path: &str) {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_git_enabled = true;
    let clock = Arc::new(TestClock::new());
    let host = SharedHost::new();
    let witness = Witness::default();
    let (entered, arrived) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let release = Release(release);
    let daemon = duded::boot(
        &settings,
        common::seams(
            &settings,
            ShutdownGate {
                host: host.clone(),
                entered,
                release: gate,
            },
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            witness.clone(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor listener");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let browser = Browser::authenticate(&settings, addr, &clock);
    let mut client = Client::connect(settings.socket()).expect("shutdown peer");
    let reply = client
        .call_as_operator(Method::SystemShutdown, None, credential(&settings))
        .expect("shutdown response");
    let stopped: ShutdownResult =
        serde_json::from_value(reply.result.expect("shutdown result")).expect("typed shutdown");
    let receipt = stopped.completion.expect("completion receipt");
    drop(client);
    let joined = std::thread::spawn(move || running.join());
    arrived
        .recv_timeout(GATE_BOUND)
        .expect("host shutdown reached");
    let steps = witness.teardown.steps();
    assert!(steps.contains(&TeardownStep::JournalFlushed));
    assert!(!steps.contains(&TeardownStep::HostShutdown));
    let observed = browser.post(path, agent);
    drop(release);
    assert_eq!(
        joined.join().expect("joined daemon"),
        duded::ExitCause::ShutdownRpc
    );
    let completion = std::fs::read_to_string(receipt.path).expect("durable completion");
    assert_eq!(
        (&observed, &completion),
        (&(503, Vec::new()), &format!("complete {}\n", receipt.run)),
        "Floor answers before host release, then teardown writes the exact completion"
    );
    assert_eq!(host.shutdowns(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::FLOOR_ORDER);
    let (status, body) = observed;
    println!(
        "{}",
        json!({
            "route": path,
            "status": status,
            "body_bytes": body.len(),
            "completion": completion,
            "host_shutdowns": host.shutdowns(),
            "teardown_steps": format!("{:?}", witness.teardown.steps()),
        })
    );
}

#[test]
fn a_memory_read_after_the_final_drain_is_unavailable_and_shutdown_completes() {
    shutdown_read("/floor/memory/page");
}

#[test]
fn a_git_read_after_the_final_drain_is_unavailable_and_shutdown_completes() {
    shutdown_read("/floor/git");
}

#[test]
fn an_unknown_git_agent_still_answers_200() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_git_enabled = true;
    let clock = Arc::new(TestClock::new());
    let daemon = duded::boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor listener");
    let mut running = common::start(daemon);
    let browser = Browser::authenticate(&settings, addr, &clock);
    let (status, body) = browser.post("/floor/git", AgentId::from_parts(1, 2));
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("Git JSON")["state"],
        "agent_unknown"
    );
    running.stop(duded::ExitCause::Ended);
}
