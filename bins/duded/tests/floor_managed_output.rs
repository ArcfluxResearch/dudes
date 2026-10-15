//! Authenticated, run-local publication from two real daemon-owned Managed children.
#![cfg(unix)]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use duded::floor_stream::FloorStreamFault;
use duded::settings::{FloorSettings, Seams};
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::ExitStatus;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_rpc::Client;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, ModelSelection, Provider, Tier};
use serde_json::Value;

mod common;
use common::{Fixture, ScriptedRunner, TestClock};

const HELPER: &str = "DUDES_FLOOR_OUTPUT_HELPER";
const LABEL: &str = "DUDES_FLOOR_OUTPUT_LABEL";
const SESSION: &str = "DUDES_FLOOR_OUTPUT_SESSION";
const DIR: &str = "DUDES_FLOOR_OUTPUT_DIR";
const ASSETS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

#[test]
fn managed_output_child_helper() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let label = std::env::var(LABEL).expect("child label");
    let session = std::env::var(SESSION).expect("child session");
    let dir = PathBuf::from(std::env::var_os(DIR).expect("child directory"));
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
    )
    .expect("init line");
    assistant(&mut out, &session, &format!("{label}-first"));
    out.flush().expect("first output flush");
    std::fs::write(dir.join("first-ready"), b"ready").expect("first marker");
    if label == "FLOOD" {
        for index in 0..300 {
            wait_file(&dir.join(format!("release-chunk-{index}")));
            assistant(&mut out, &session, &format!("FLOOD-{index}"));
            out.flush().expect("flood output flush");
            std::fs::write(dir.join(format!("wrote-chunk-{index}")), b"ready")
                .expect("flood marker");
        }
        wait_file(&dir.join("release-exit"));
        return;
    }
    wait_file(&dir.join("release-next"));
    assistant(&mut out, &session, &format!("{label}-second"));
    writeln!(out, r#"{{"type":"assistant","session_id":"wrong","message":{{"role":"assistant","content":[{{"type":"text","text":"{label}-wrong-session"}}]}}}}"#).expect("wrong-session line");
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{label}-thought"}}]}}}}"#).expect("thinking line");
    out.flush().expect("second output flush");
    std::fs::write(dir.join("second-ready"), b"ready").expect("second marker");
    if label == "A" {
        wait_file(&dir.join("release-third"));
        assistant(&mut out, &session, "A-third");
        out.flush().expect("third output flush");
        std::fs::write(dir.join("third-ready"), b"ready").expect("third marker");
    }
    wait_file(&dir.join("release-exit"));
    writeln!(out, r#"{{"type":"result","session_id":"{session}","subtype":"success","is_error":false,"result":"{label}-result"}}"#).expect("result line");
    out.flush().expect("end flush");
}

fn assistant(out: &mut impl std::io::Write, session: &str, text: &str) {
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#).expect("assistant line");
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child wait timed out: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct LocalAdapter;
impl Adapter for LocalAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }
    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }
    fn isolation(&self) -> Isolation {
        Isolation::None
    }
    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }
    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }
    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        let session = session
            .id()
            .ok_or(dudes_providers::launch::LaunchError::SessionModeMismatch)?;
        Ok(LaunchSpec {
            argv: [
                OsString::from(std::env::current_exe().expect("test binary")),
                "--exact".into(),
                "managed_output_child_helper".into(),
                "--quiet".into(),
            ]
            .into(),
            env: [
                EnvVar::new(HELPER, "1"),
                EnvVar::new(LABEL, ctx.cwd.file_name().expect("child label")),
                EnvVar::new(SESSION, session.as_str()),
                EnvVar::new(DIR, ctx.cwd.as_os_str()),
            ]
            .into(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }
    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }
    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Child lines the daemon has finished reading. A child's marker file says only that it wrote a
/// line; a line recorded here has already been captured into the daemon's output registry.
#[derive(Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
    changed: Condvar,
}
impl Captured {
    fn wait_for(&self, text: &str) {
        let lines = self.lines.lock().unwrap();
        let (lines, _) = self
            .changed
            .wait_timeout_while(lines, Duration::from_secs(20), |lines| {
                !lines.iter().any(|line| line.contains(text))
            })
            .unwrap();
        assert!(
            lines.iter().any(|line| line.contains(text)),
            "the daemon never read {text}"
        );
    }
}

struct HostStreams(Arc<ChildEnds>, Arc<Captured>);
impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostChildTurn(self.0.watch(key), Arc::clone(&self.1)))
    }
}
struct HostChildTurn(ChildTurn, Arc<Captured>);
impl ChildStream for HostChildTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
        self.1.lines.lock().unwrap().push(text.to_owned());
        self.1.changed.notify_all();
    }
    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.ended_with_elapsed(
            ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn settings(fixture: &Fixture, port: u16) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: ASSETS.into(),
        port: Some(port),
    });
    settings
}

fn seams(
    settings: &duded::Settings,
    children: Arc<ChildEnds>,
    captured: Arc<Captured>,
) -> Seams<RealHost, ScriptedRunner, TestClock, common::FixtureRng> {
    Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children), captured))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([
            common::passes(),
            common::passes(),
        ])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    }
}

fn issue(settings: &duded::Settings) -> SessionIssueResult {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let mut client = Client::connect(settings.socket()).expect("daemon RPC");
    let response = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(credential).unwrap(),
        )
        .expect("session issue");
    serde_json::from_value(response.result.expect("issue result")).expect("typed issue")
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("HTTP head");
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

fn response_body(stream: &mut TcpStream, head: &str) -> Value {
    let len: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap_or("0")
        .parse()
        .unwrap();
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).expect("HTTP body");
    serde_json::from_slice(&bytes).expect("JSON body")
}

fn exchange(addr: SocketAddr, bootstrap: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{bootstrap}",
        bootstrap.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let head = read_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|v| v.split(';').next())
        .unwrap()
        .to_owned();
    let body = response_body(&mut stream, &head);
    (cookie, body["csrf"].as_str().unwrap().to_owned())
}

fn upgrade(
    addr: SocketAddr,
    cookie: Option<&str>,
    csrf: Option<&str>,
    origin: Option<&str>,
) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {addr}\r\nOrigin: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        origin.unwrap_or(&format!("http://{addr}"))
    );
    if let Some(csrf) = csrf {
        write!(
            request,
            "Sec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\n"
        )
        .unwrap();
    }
    if let Some(cookie) = cookie {
        write!(request, "Cookie: dudes_floor_session={cookie}\r\n").unwrap();
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let head = read_head(&mut stream);
    (stream, head)
}

fn upgrade_with_authority_headers(
    addr: SocketAddr,
    host: Option<&str>,
    origin: Option<&str>,
    cookie: &str,
    csrf: &str,
) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = String::from("GET /floor/stream HTTP/1.1\r\n");
    if let Some(host) = host {
        write!(request, "Host: {host}\r\n").unwrap();
    }
    if let Some(origin) = origin {
        write!(request, "Origin: {origin}\r\n").unwrap();
    }
    write!(request, "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n").unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    read_head(&mut stream)
}

struct WebSocket(TcpStream);
impl WebSocket {
    fn connect(addr: SocketAddr, cookie: &str, csrf: &str) -> Self {
        let (stream, head) = upgrade(addr, Some(cookie), Some(csrf), None);
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols"),
            "{head}"
        );
        Self(stream)
    }
    fn send(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mut frame = vec![0x81];
        match payload.len() {
            0..=125 => frame.push(0x80 | u8::try_from(payload.len()).unwrap()),
            126..=65_535 => {
                frame.push(0x80 | 0x7e);
                frame.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
            }
            _ => {
                frame.push(0x80 | 0x7f);
                frame.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_be_bytes());
            }
        }
        let mask = [1_u8, 2, 3, 4];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.0.write_all(&frame).unwrap();
    }
    fn json(&mut self) -> Value {
        let mut head = [0_u8; 2];
        self.0.read_exact(&mut head).expect("WebSocket head");
        assert_eq!(head[0] & 0x0f, 1, "expected text frame");
        let len = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0; 2];
                self.0.read_exact(&mut bytes).unwrap();
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0; 8];
                self.0.read_exact(&mut bytes).unwrap();
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
        };
        let mut body = vec![0; len];
        self.0.read_exact(&mut body).expect("WebSocket body");
        serde_json::from_slice(&body).expect("server JSON")
    }
    fn output(&mut self) -> Value {
        loop {
            let frame = self.json();
            if frame["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("floor.output."))
            {
                return frame;
            }
        }
    }
    fn text(&mut self, expected: &str) -> Value {
        for _ in 0..12 {
            let frame = self.output();
            if frame.to_string().contains(expected) {
                return frame;
            }
            assert!(!frame.to_string().contains("wrong-session"));
        }
        panic!("selected output never contained {expected}");
    }
    fn expect_closed(&mut self) {
        let mut byte = [0];
        match self.0.read(&mut byte) {
            Ok(0) => {}
            Ok(1) if byte[0] == 0x88 => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) => {}
            other => panic!("invalid browser message left socket open: {other:?}"),
        }
    }
}

#[test]
fn selected_reply_arrives_before_child_completion_and_reconnects() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let captured = Arc::new(Captured::default());
    let daemon = duded::boot::boot_with_options_and_floor_faults(
        &settings,
        seams(&settings, ChildEnds::new(), Arc::clone(&captured)),
        duded::settings::RunOptions::default(),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .expect("boot local reply fixture");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let dir = fixture.state.path().join("REPLY");
    std::fs::create_dir(&dir).unwrap();
    let agent = common::hire(&running.handle, &dir);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut socket = WebSocket::connect(addr, &cookie, &csrf);
    socket.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(socket.json()["type"], "floor.snapshot");
    let select = format!(r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#);
    socket.send(&select);
    assert_eq!(socket.output()["reason"], "no_run_output");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    wait_file(&dir.join("first-ready"));
    let first = socket.text("REPLY-first");
    assert_eq!(first["type"], "floor.output.reply");
    assert_eq!(first["text"], "REPLY-first");
    assert_eq!(first["ended"], false);
    assert_eq!(first["truncated"], false);
    assert_eq!(first["gap"], Value::Null);
    assert!(!dir.join("release-exit").exists());
    std::fs::write(dir.join("release-next"), b"go").unwrap();
    wait_file(&dir.join("second-ready"));
    captured.wait_for("REPLY-thought");
    let second = socket.text("REPLY-second");
    let text = second["text"].as_str().unwrap();
    assert!(text.starts_with("REPLY-first") && text.ends_with("REPLY-second"));
    assert!(!text.contains("wrong-session") && !text.contains("thought"));
    assert_eq!(second["ended"], false);
    let revision = |frame: &Value| frame["revision"].as_str().unwrap().parse::<u128>().unwrap();
    assert!(revision(&second) > revision(&first));
    let mut reconnect = WebSocket::connect(addr, &cookie, &csrf);
    reconnect.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(reconnect.json()["type"], "floor.snapshot");
    reconnect.send(&select);
    let restored = reconnect.output();
    assert_eq!(restored["text"], second["text"]);
    assert_eq!(restored["turn_id"], first["turn_id"]);
    assert!(revision(&restored) >= revision(&second));
    std::fs::write(dir.join("release-exit"), b"go").unwrap();
    let ended = loop {
        let frame = reconnect.output();
        if frame["ended"] == true {
            break frame;
        }
    };
    assert_eq!(ended["text"], second["text"]);
    assert!(revision(&ended) > revision(&second));
    assert_eq!(ended["agent_id"], serde_json::json!(agent));
    drop(socket);
    drop(reconnect);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[allow(clippy::too_many_lines)]
fn selected_output_read_is_epoch_scoped_and_bounded() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let children = ChildEnds::new();
    let captured = Arc::new(Captured::default());
    let fault = FloorStreamFault::default().without_heartbeat();
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        seams(&settings, Arc::clone(&children), Arc::clone(&captured)),
        None,
        fault.clone(),
    )
    .expect("boot real-child Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let a_dir = fixture.state.path().join("A");
    let b_dir = fixture.state.path().join("B");
    std::fs::create_dir(&a_dir).unwrap();
    std::fs::create_dir(&b_dir).unwrap();
    let a: AgentId = common::hire(&running.handle, &a_dir);
    let b: AgentId = common::hire(&running.handle, &b_dir);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut first = WebSocket::connect(addr, &cookie, &csrf);
    let mut second = WebSocket::connect(addr, &cookie, &csrf);
    first.send(r#"{"type":"floor.subscribe"}"#);
    second.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(first.json()["type"], "floor.snapshot");
    assert_eq!(second.json()["type"], "floor.snapshot");
    first.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{a}"}}"#
    ));
    second.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{b}"}}"#
    ));
    let held_old_status = first.output();
    assert_eq!(held_old_status["reason"], "no_run_output");
    assert_eq!(second.output()["reason"], "no_run_output");
    let (_third, third_head) = upgrade(addr, Some(&cookie), Some(&csrf), None);
    assert!(
        third_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{third_head}"
    );

    common::set_goal(&running.handle, common::goal_params(a, &["/usr/bin/true"]));
    common::set_goal(&running.handle, common::goal_params(b, &["/usr/bin/true"]));
    wait_file(&a_dir.join("first-ready"));
    wait_file(&b_dir.join("first-ready"));
    let a_first = first.text("A-first");
    let b_first = second.text("B-first");
    assert_eq!(a_first["agent_id"], serde_json::json!(a));
    assert_eq!(b_first["agent_id"], serde_json::json!(b));
    assert!(!a_first.to_string().contains("B-first"));
    assert!(!b_first.to_string().contains("A-first"));
    let a_turn = a_first["turn_id"].clone();
    std::fs::write(a_dir.join("release-next"), b"go").unwrap();
    wait_file(&a_dir.join("second-ready"));
    let held_old_chunk = first.text("A-second");
    assert_eq!(held_old_chunk["selection_epoch"], 1);
    first.send(r#"{"type":"floor.output.clear"}"#);
    let clear = first.output();
    assert_eq!(clear["reason"], "cleared");
    assert_eq!(clear["selection_epoch"], 2);
    first.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{a}"}}"#
    ));
    let a_again = first.output();
    assert_eq!(a_again["type"], "floor.output.snapshot");
    assert_eq!(a_again["selection_epoch"], 3);
    assert_eq!(a_again["turn_id"], a_turn);
    assert!(a_again.to_string().contains("A-second"));
    assert!(!a_again.to_string().contains("B-first"));
    assert_eq!(held_old_status["selection_epoch"], 1);
    assert_eq!(held_old_chunk["selection_epoch"], 1);

    // One A watcher pass is past its wake check when the hold is armed, as load can leave it.
    // A pass that began before the reselect carries epoch 1 and could read nothing, so one held
    // wake goes by first and the parked pass carries epoch 3. The hold then engages before A
    // writes its third line, so the parked pass cannot be the one to read it.
    fault.hold_next_output_wake(a);
    assert!(
        fault.wait_output_wake(Duration::from_secs(2)),
        "no A wake after the reselect"
    );
    fault.pause_next_output_pass(a);
    fault.release_output_wake();
    assert!(
        fault.wait_output_pass(Duration::from_secs(2)),
        "no A watcher pass passed its wake check"
    );
    fault.hold_next_output_wake(a);
    fault.release_output_pass();
    assert!(
        fault.wait_output_wake(Duration::from_secs(2)),
        "late A wake was not held"
    );
    std::fs::write(a_dir.join("release-third"), b"go").unwrap();
    wait_file(&a_dir.join("third-ready"));
    captured.wait_for("A-third");
    first.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{b}"}}"#
    ));
    let switched_to_b = first.output();
    assert_eq!(switched_to_b["type"], "floor.output.snapshot");
    assert_eq!(switched_to_b["selection_epoch"], 4);
    assert_eq!(switched_to_b["agent_id"], serde_json::json!(b));
    assert!(switched_to_b.to_string().contains("B-first"));
    assert!(!switched_to_b.to_string().contains("A-third"));
    fault.release_output_wake();

    std::fs::write(b_dir.join("release-next"), b"go").unwrap();
    wait_file(&b_dir.join("second-ready"));
    let b_on_switched_socket = first.output();
    assert_eq!(b_on_switched_socket["type"], "floor.output.chunk");
    assert_eq!(b_on_switched_socket["selection_epoch"], 4);
    assert_eq!(b_on_switched_socket["agent_id"], serde_json::json!(b));
    assert_eq!(b_on_switched_socket["text"], "\nB-second");
    let b_second = second.text("B-second");
    assert_eq!(b_second["selection_epoch"], 1);
    assert!(!b_second.to_string().contains("A-second"));
    first.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{a}"}}"#
    ));
    let back_to_a = first.output();
    assert_eq!(back_to_a["type"], "floor.output.snapshot");
    assert_eq!(back_to_a["selection_epoch"], 5);
    assert_eq!(back_to_a["agent_id"], serde_json::json!(a));
    assert!(back_to_a.to_string().contains("A-third"));
    assert!(!back_to_a.to_string().contains("B-second"));

    drop(first);
    drop(second);
    let deadline = Instant::now() + Duration::from_secs(3);
    let fresh_csrf = loop {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = format!(
            "POST /floor/session/recover HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nCookie: dudes_floor_session={cookie}\r\nContent-Length: 0\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let head = read_head(&mut stream);
        if head.starts_with("HTTP/1.1 200 OK") {
            break response_body(&mut stream, &head)["csrf"]
                .as_str()
                .unwrap()
                .to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "recovery never released sockets: {head}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(fresh_csrf, csrf);
    let mut stolen_first = WebSocket::connect(addr, &cookie, &fresh_csrf);
    let mut stolen_second = WebSocket::connect(addr, &cookie, &fresh_csrf);
    for (socket, agent) in [(&mut stolen_first, a), (&mut stolen_second, b)] {
        socket.send(r#"{"type":"floor.subscribe"}"#);
        assert_eq!(socket.json()["type"], "floor.snapshot");
        socket.send(&format!(
            r#"{{"type":"floor.output.select","agent_id":"{agent}"}}"#
        ));
    }
    assert!(
        stolen_first
            .text("A-second")
            .to_string()
            .contains("A-first")
    );
    assert!(
        stolen_second
            .text("B-second")
            .to_string()
            .contains("B-first")
    );
    let (_wrong, wrong_head) = upgrade(addr, None, Some(&fresh_csrf), None);
    assert!(
        wrong_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{wrong_head}"
    );
    let (_wrong_origin, origin_head) = upgrade(
        addr,
        Some(&cookie),
        Some(&fresh_csrf),
        Some("http://wrong.invalid"),
    );
    assert!(
        origin_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{origin_head}"
    );
    for (host, origin, status) in [
        (None, Some(format!("http://{addr}")), "400 Bad Request"),
        (
            Some("wrong.invalid".to_owned()),
            Some(format!("http://{addr}")),
            "403 Forbidden",
        ),
        (Some(addr.to_string()), None, "403 Forbidden"),
    ] {
        let head = upgrade_with_authority_headers(
            addr,
            host.as_deref(),
            origin.as_deref(),
            &cookie,
            &fresh_csrf,
        );
        assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{head}");
    }

    std::fs::write(a_dir.join("release-exit"), b"go").unwrap();
    std::fs::write(b_dir.join("release-exit"), b"go").unwrap();
    assert_eq!(stolen_first.output()["type"], "floor.output.ended");
    assert_eq!(stolen_second.output()["type"], "floor.output.ended");
    drop(stolen_first);
    drop(stolen_second);
    for (cookie_value, csrf_value) in [
        (None, Some(fresh_csrf.as_str())),
        (Some("wrong-cookie"), Some(fresh_csrf.as_str())),
        (Some(cookie.as_str()), None),
        (Some(cookie.as_str()), Some("wrong-csrf")),
    ] {
        let (_socket, head) = upgrade(addr, cookie_value, csrf_value, None);
        assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    }
    for invalid in [
        r#"{"type":"floor.output.select","agent_id":"bad","command":"write"}"#,
        r#"{"type":"floor.terminal.write","text":"x"}"#,
        r#"{"type":"agent.hire"}"#,
        r#"{"type":"goal.set"}"#,
        r#"{"type":"human.answer"}"#,
        r#"{"jsonrpc":"2.0","method":"agent.hire"}"#,
    ] {
        let mut socket = WebSocket::connect(addr, &cookie, &fresh_csrf);
        socket.send(r#"{"type":"floor.subscribe"}"#);
        assert_eq!(socket.json()["type"], "floor.snapshot");
        socket.send(invalid);
        socket.expect_closed();
    }

    let flood_dir = fixture.state.path().join("FLOOD");
    std::fs::create_dir(&flood_dir).unwrap();
    let flood = common::hire(&running.handle, &flood_dir);
    fault.hold_next_subscription();
    let mut flooded = WebSocket::connect(addr, &cookie, &fresh_csrf);
    flooded.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));
    fault.release();
    assert_eq!(flooded.json()["type"], "floor.snapshot");
    fault.hold_next_output_drain();
    flooded.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{flood}"}}"#
    ));
    assert!(fault.wait_output_drain(Duration::from_secs(2)));
    common::set_goal(
        &running.handle,
        common::goal_params(flood, &["/usr/bin/true"]),
    );
    wait_file(&flood_dir.join("first-ready"));
    let mut overflowed = false;
    for index in 0..300 {
        let before = fault.queue_usage().expect("held flood queue").0;
        std::fs::write(flood_dir.join(format!("release-chunk-{index}")), b"go").unwrap();
        wait_file(&flood_dir.join(format!("wrote-chunk-{index}")));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (frames, _, full) = fault.queue_usage().expect("held flood queue");
            if full {
                overflowed = true;
                break;
            }
            if frames > before {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "flood output did not reach queue"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        if overflowed {
            break;
        }
    }
    assert!(
        overflowed,
        "the output flood never closed the 256-frame queue"
    );
    fault.release_output_drain();
    flooded.expect_closed();
    drop(flooded);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let restarted = boot(
        &settings,
        seams(&settings, ChildEnds::new(), Arc::default()),
    )
    .expect("restart from journal");
    let new_addr = restarted.floor_addr().expect("new Floor address");
    let mut running = common::start(restarted);
    let (_old, old_head) = upgrade(new_addr, Some(&cookie), Some(&fresh_csrf), None);
    assert!(old_head.starts_with("HTTP/1.1 403 Forbidden"), "{old_head}");
    let issued = issue(&settings);
    let (new_cookie, new_csrf) = exchange(new_addr, &issued.bootstrap);
    let mut reloaded = WebSocket::connect(new_addr, &new_cookie, &new_csrf);
    reloaded.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(reloaded.json()["type"], "floor.snapshot");
    reloaded.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{a}"}}"#
    ));
    let after_restart = reloaded.output();
    assert_eq!(after_restart["type"], "floor.output.unavailable");
    assert_eq!(after_restart["reason"], "no_run_output");
    assert!(!after_restart.to_string().contains("A-first"));
    drop(reloaded);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
