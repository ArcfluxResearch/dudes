//! Authenticated Floor reads during the daemon's host-shutdown boundary.
#![cfg(unix)]

mod common;

use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};
use duded::floor_session::CSRF_HEADER_NAME;
use duded::settings::FloorSettings;
use duded::startup::{TeardownStep, Witness};
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_proto::rpc::system::ShutdownResult;
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_rpc::Client;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};
use dudes_types::AgentId;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);
const HTTP_BOUND: Duration = Duration::from_secs(10);
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

struct Browser {
    addr: SocketAddr,
    cookie: String,
    csrf: String,
}

fn response(stream: &mut TcpStream) -> io::Result<(u16, Vec<u8>)> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
        if head.len() > 16 * 1024 {
            return Err(io::Error::other("response head is too large"));
        }
    }
    let head =
        String::from_utf8(head).map_err(|_| io::Error::other("response head is not UTF-8"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("response has no status"))?;
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("response has no length"))?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok((status, body))
}

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
    fn authenticate(settings: &duded::Settings, addr: SocketAddr) -> Self {
        let mut client = Client::connect(settings.socket()).expect("control socket");
        let reply = client
            .call_as_operator(Method::FloorSessionIssue, None, credential(settings))
            .expect("session issue");
        let issued: SessionIssueResult =
            serde_json::from_value(reply.result.expect("session result")).expect("typed session");
        drop(client);
        let mut stream = TcpStream::connect(addr).expect("Floor listener");
        stream
            .set_read_timeout(Some(HTTP_BOUND))
            .expect("read bound");
        let request = format!(
            "POST /floor/session HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{}",
            issued.bootstrap.len(),
            issued.bootstrap
        );
        stream
            .write_all(request.as_bytes())
            .expect("session exchange");
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("session response head");
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).expect("session head");
        assert!(head.starts_with("HTTP/1.1 200 OK"), "session was refused");
        let cookie = head
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
            .and_then(|value| value.split(';').next())
            .expect("session cookie")
            .to_owned();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.parse().ok())
            .expect("session response length");
        let mut body = vec![0; length];
        stream.read_exact(&mut body).expect("session response body");
        let csrf = serde_json::from_slice::<Value>(&body).expect("session JSON")["csrf"]
            .as_str()
            .expect("CSRF nonce")
            .to_owned();
        Self { addr, cookie, csrf }
    }

    fn post(&self, path: &str, agent: AgentId) -> io::Result<(u16, Vec<u8>)> {
        let body = if path == "/floor/memory/page" {
            json!({"agent_id": agent.to_string(), "before": null})
        } else {
            json!({"agent_id": agent.to_string()})
        }
        .to_string();
        let addr = self.addr;
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: application/json\r\nCookie: dudes_floor_session={}\r\n{CSRF_HEADER_NAME}: {}\r\nContent-Length: {}\r\n\r\n{body}",
            self.cookie,
            self.csrf,
            body.len()
        );
        let mut stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(HTTP_BOUND))?;
        stream.write_all(request.as_bytes())?;
        response(&mut stream)
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
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor listener");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let browser = Browser::authenticate(&settings, addr);
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
    let observed = observed.map_err(|error| error.kind());
    assert_eq!(
        (&observed, &completion),
        (
            &Ok((503, Vec::new())),
            &format!("complete {}\n", receipt.run)
        ),
        "Floor answers before host release, then teardown writes the exact completion"
    );
    assert_eq!(host.shutdowns(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::FLOOR_ORDER);
    let (status, body) = observed.expect("validated response");
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
    let daemon = duded::boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor listener");
    let mut running = common::start(daemon);
    let browser = Browser::authenticate(&settings, addr);
    let (status, body) = browser
        .post("/floor/git", AgentId::from_parts(1, 2))
        .expect("unknown agent response");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("Git JSON")["state"],
        "agent_unknown"
    );
    running.stop(duded::ExitCause::Ended);
}
