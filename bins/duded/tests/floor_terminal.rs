//! The checks for the browser terminal, driven through the real Floor gateway by a
//! scripted browser against the fake PTY port.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, ScriptedRunner, SharedHost, SharedPtyHost, TestClock};
use duded::floor_gateway::FloorGatewayProbe;
use duded::floor_session::FloorSessionAuthorizer;
use duded::floor_terminal::{SkewField, TERMINAL_QUEUE_BYTES, TerminalSkew};
use duded::ports::{Answered, Ask};
use duded::runtime::{
    AppendFailure, OnboardingFailure, OnboardingFailurePoint, OnboardingPause, OnboardingPausePoint,
};
use duded::settings::{FloorControl, FloorSettings};
use duded::startup::Witness;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::agent::RetireParams;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_proto::rpc::pty::PtyId;
use dudes_proto::{Actor, Entry, ExitStatus, PeerProvenance, Record, TurnOutcome};
use dudes_rpc::Client;
use dudes_rpc::methods::pty::decode_terminal_input;
use dudes_supervisor::Input;
use dudes_types::AgentId;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);
const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);
const WIRE_BOUND_BYTES: usize = 8 * 1024 * 1024;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
const TERMINAL_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

struct Floor {
    running: common::Running,
    settings: duded::Settings,
    addr: SocketAddr,
    host: SharedHost,
    ptys: SharedPtyHost,
    clock: Arc<TestClock>,
    skew: TerminalSkew,
    probe: FloorGatewayProbe,
    post_fsync_failure: OnboardingFailure,
    append_failure: AppendFailure,
    pause: OnboardingPause,
    sessions: FloorSessionAuthorizer,
}

fn floor(fixture: &Fixture, terminal: bool) -> Floor {
    floor_with(fixture, terminal, FloorControl::Off)
}

fn floor_with(fixture: &Fixture, terminal: bool, control: FloorControl) -> Floor {
    floor_reading(fixture, terminal, control, None)
}

/// [`floor_with`] on a daemon that reloads `config`, the operator file.
fn floor_reading(
    fixture: &Fixture,
    terminal: bool,
    control: FloorControl,
    config: Option<PathBuf>,
) -> Floor {
    let mut settings = fixture.settings();
    settings.operator_config = config;
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_terminal_enabled = terminal;
    settings.floor_control = control;
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let clock = Arc::new(TestClock::new());
    let mut seams = common::seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::passes()])),
        Arc::clone(&clock),
        Witness::default(),
    );
    seams.pty_host = Box::new(ptys.clone());
    seams.resolver = common::native_resolver(&ptys);
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    let skew = daemon.terminal_skew();
    let probe = daemon.floor_gateway_probe().expect("the Floor probe");
    let post_fsync_failure = daemon.onboarding_failure();
    let append_failure = daemon.append_failure();
    let pause = daemon.onboarding_pause();
    let sessions = daemon
        .floor_session_authorizer()
        .expect("the Floor sessions");
    Floor {
        running: common::start(daemon),
        settings,
        addr,
        host,
        ptys,
        clock,
        skew,
        probe,
        post_fsync_failure,
        append_failure,
        pause,
        sessions,
    }
}

impl Floor {
    fn stop(&mut self) {
        assert_eq!(
            self.running.stop(duded::ExitCause::Ended),
            duded::ExitCause::Ended
        );
    }

    fn await_connections(&self, want: usize) {
        let deadline = Instant::now() + WAIT;
        while self.probe.live_connections() != want {
            assert!(
                Instant::now() < deadline,
                "{} Floor connections, not {want}",
                self.probe.live_connections()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn credential(settings: &duded::Settings) -> String {
    std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential")
}

/// Issue and exchange a Floor bootstrap: the cookie and the CSRF nonce a signed-in page holds.
fn session(floor: &Floor) -> (String, String) {
    let mut client = Client::connect(floor.settings.socket()).expect("daemon socket");
    let reply = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(credential(&floor.settings)).expect("credential"),
        )
        .expect("issue response");
    let issued: SessionIssueResult =
        serde_json::from_value(reply.result.expect("issue succeeded")).expect("typed issue");
    let host = floor.addr.to_string();
    let mut stream = TcpStream::connect(floor.addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{}",
        issued.bootstrap.len(),
        issued.bootstrap
    );
    stream.write_all(request.as_bytes()).expect("exchange");
    let head = read_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("cookie")
        .to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("exchange body");
    let csrf = serde_json::from_slice::<Value>(&body).expect("JSON")["csrf"]
        .as_str()
        .expect("csrf")
        .to_owned();
    (cookie, csrf)
}

/// Hire an agent and confirm its vendor session, which attach requires.
fn attachable(floor: &Floor, fixture: &Fixture) -> AgentId {
    let handle = &floor.running.handle;
    let cwd = tempfile::tempdir_in(fixture.state.path())
        .expect("agent working directory")
        .keep();
    let agent = common::hire(handle, &cwd);
    common::set_goal(handle, common::goal_params(agent, &["/usr/bin/true"]));
    let (turn, session) = fixture.await_record("TurnIssued", |record| match record {
        Record::TurnIssued {
            agent: seen,
            turn,
            session: Some(session),
            ..
        } if *seen == agent => Some((*turn, session.clone())),
        _ => None,
    });
    handle.input(Input::SessionLearned { turn, session });
    fixture.await_record("SessionLearned", |record| match record {
        Record::SessionLearned { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    common::end_turn_with_native_payload(
        fixture,
        &floor.host,
        Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        },
    );
    fixture.await_record("PredicateResult", |record| match record {
        Record::PredicateResult {
            turn: Some(seen), ..
        } if *seen == turn => Some(()),
        _ => None,
    });
    agent
}

fn count(fixture: &Fixture, select: impl Fn(&Record) -> bool) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| select(record))
        .count()
}

fn await_system_detach(fixture: &Fixture, agent: AgentId, want: usize) {
    let deadline = Instant::now() + WAIT;
    while count(
        fixture,
        |record| matches!(record, Record::Detached { agent: seen, by: Actor::System, provenance: None } if *seen == agent),
    ) < want
    {
        assert!(
            Instant::now() < deadline,
            "system Detached {want} did not arrive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn host_bytes(ptys: &SharedPtyHost) -> Vec<Vec<u8>> {
    ptys.writes().into_iter().map(|(_, bytes)| bytes).collect()
}

fn await_write(ptys: &SharedPtyHost, bytes: &[u8]) {
    let deadline = Instant::now() + WAIT;
    while !host_bytes(ptys).iter().any(|written| written == bytes) {
        assert!(
            Instant::now() < deadline,
            "{bytes:?} never reached the host"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn newest_pty(ptys: &SharedPtyHost) -> PtyId {
    ptys.live().into_iter().next_back().expect("an open PTY")
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

fn upgrade(addr: SocketAddr, path: &str, protocol: &str, cookie: &str, csrf: &str) -> String {
    let host = addr.to_string();
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
    )
}

fn raw_response(addr: SocketAddr, request: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    stream.write_all(request.as_bytes()).expect("request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn recover(floor: &Floor, cookie: &str) -> String {
    let host = floor.addr.to_string();
    let response = raw_response(
        floor.addr,
        &format!(
            "POST /floor/session/recover HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nContent-Length: 0\r\n\r\n"
        ),
    );
    String::from_utf8_lossy(&response).into_owned()
}

#[derive(Debug, PartialEq)]
enum Frame {
    Message(Value),
    Close(Option<u16>),
    Gone,
}

#[derive(Debug)]
struct Socket {
    stream: TcpStream,
    texts: Vec<String>,
}

impl Socket {
    fn open(
        addr: SocketAddr,
        path: &str,
        protocol: &str,
        cookie: &str,
        csrf: &str,
    ) -> Result<Self, String> {
        let mut stream = TcpStream::connect(addr).expect("Floor listener");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        stream
            .write_all(upgrade(addr, path, protocol, cookie, csrf).as_bytes())
            .expect("upgrade");
        let head = read_head(&mut stream);
        if !head.starts_with("HTTP/1.1 101 Switching Protocols") {
            return Err(head);
        }
        assert!(
            head.contains(&format!("Sec-WebSocket-Protocol: {protocol}\r\n")),
            "{head}"
        );
        Ok(Self {
            stream,
            texts: Vec::new(),
        })
    }

    /// Retries a 403 briefly: a channel that just closed may still hold its slot.
    fn open_retrying(floor: &Floor, path: &str, protocol: &str, cookie: &str, csrf: &str) -> Self {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Self::open(floor.addr, path, protocol, cookie, csrf) {
                Ok(socket) => return socket,
                Err(head) if head.starts_with("HTTP/1.1 403") && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(head) => panic!("{path} did not upgrade: {head}"),
            }
        }
    }

    fn terminal(floor: &Floor, cookie: &str, csrf: &str) -> Self {
        Self::open_retrying(floor, "/floor/terminal", "dudes-terminal-v1", cookie, csrf)
    }

    fn projection(floor: &Floor, cookie: &str, csrf: &str) -> Self {
        let mut socket =
            Self::open_retrying(floor, "/floor/stream", "dudes-floor-v3", cookie, csrf);
        socket.send(&json!({"type": "floor.subscribe"}));
        socket.expect("floor.snapshot");
        socket
    }

    fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mask = [0x5a_u8, 0xa5, 0x3c, 0xc3];
        let mut frame = vec![0x81];
        match payload.len() {
            len @ 0..=125 => frame.push(0x80 | u8::try_from(len).expect("short")),
            len @ 126..=65_535 => {
                frame.push(0x80 | 0x7e);
                frame.extend(u16::try_from(len).expect("medium").to_be_bytes());
            }
            len => {
                frame.push(0x80 | 0x7f);
                frame.extend(u64::try_from(len).expect("long").to_be_bytes());
            }
        }
        frame.extend(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        let _ = self.stream.write_all(&frame);
    }

    fn send(&mut self, value: &Value) {
        self.send_text(&value.to_string());
    }

    fn close(&mut self) {
        let _ = self.stream.write_all(&[0x88, 0x80, 1, 2, 3, 4]);
    }

    fn next_within(&mut self, wait: Duration) -> Option<Frame> {
        self.read_timeout(wait);
        let mut head = [0_u8; 2];
        match self.stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return None;
            }
            Err(_) => return Some(Frame::Gone),
        }
        Some(self.read_frame(head))
    }

    fn read_frame(&mut self, head: [u8; 2]) -> Frame {
        let length = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0; 2];
                self.stream.read_exact(&mut bytes).expect("u16 length");
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0; 8];
                self.stream.read_exact(&mut bytes).expect("u64 length");
                usize::try_from(u64::from_be_bytes(bytes)).expect("bounded frame")
            }
        };
        let mut payload = vec![0; length];
        self.read_timeout(WAIT);
        self.stream.read_exact(&mut payload).expect("frame payload");
        match head[0] {
            0x81 => {
                let text = String::from_utf8(payload).expect("UTF-8 text");
                self.texts.push(text.clone());
                Frame::Message(serde_json::from_str(&text).expect("JSON message"))
            }
            0x88 => Frame::Close(
                (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]])),
            ),
            other => panic!("unexpected opcode {other:#x}"),
        }
    }

    fn read_timeout(&self, wait: Duration) {
        assert!(!wait.is_zero(), "the read timeout must be positive");
        match self.stream.set_read_timeout(Some(wait)) {
            Ok(()) => {}
            #[cfg(target_os = "macos")]
            Err(error) if error.raw_os_error() == Some(nix::libc::EINVAL) => {
                // macOS refuses option changes after both directions end, even with queued bytes.
            }
            Err(error) => panic!("timeout: {error}"),
        }
    }

    fn next(&mut self) -> Frame {
        self.next_within(WAIT).expect("a frame within the wait")
    }

    fn expect(&mut self, kind: &str) -> Value {
        match self.next() {
            Frame::Message(message) if message["type"] == kind => message,
            other => panic!("expected {kind}, got {other:?}"),
        }
    }

    fn expect_policy_close(&mut self) {
        loop {
            match self.next() {
                Frame::Close(code) => {
                    assert_eq!(code, Some(1008));
                    return;
                }
                Frame::Message(message) if message["type"] == "terminal.data" => {}
                other => panic!("expected a policy close, got {other:?}"),
            }
        }
    }

    fn expect_terminal_closed(&mut self) {
        loop {
            match self.next() {
                Frame::Message(message) if message["type"] == "terminal.closed" => break,
                Frame::Message(message) if message["type"] == "terminal.data" => {}
                other => panic!("expected terminal.closed, got {other:?}"),
            }
        }
        assert_eq!(self.next(), Frame::Close(None));
    }

    fn expect_any_close(&mut self) {
        loop {
            match self.next() {
                Frame::Close(_) | Frame::Gone => return,
                Frame::Message(message)
                    if matches!(
                        message["type"].as_str(),
                        Some("terminal.closed" | "terminal.data")
                    ) => {}
                other @ Frame::Message(_) => panic!("expected the channel to close, got {other:?}"),
            }
        }
    }

    fn attach(&mut self, agent: AgentId) -> String {
        self.send(&json!({"type": "terminal.attach", "agent": agent, "cols": 80, "rows": 24}));
        self.expect("terminal.attached")["handle"]
            .as_str()
            .expect("a handle")
            .to_owned()
    }

    fn write(&mut self, handle: &str, bytes: &[u8]) {
        self.send(&json!({
            "type": "terminal.write",
            "handle": handle,
            "data": common::base64_standard(bytes),
        }));
    }

    fn data(&mut self) -> Vec<u8> {
        let data = self.expect("terminal.data")["data"]
            .as_str()
            .expect("data")
            .to_owned();
        decode_terminal_input(&data)
            .expect("terminal.data is canonical base64")
            .0
    }
}

fn socket_pair() -> (Socket, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let stream = TcpStream::connect(listener.local_addr().expect("loopback address"))
        .expect("loopback client");
    let (peer, _) = listener.accept().expect("loopback peer");
    stream
        .set_read_timeout(Some(WAIT))
        .expect("initial timeout");
    (
        Socket {
            stream,
            texts: Vec::new(),
        },
        peer,
    )
}

#[cfg(target_os = "macos")]
#[test]
fn buffered_frames_are_read_after_a_peer_reset() {
    for (wire, expected) in [
        (&b"\x81\x02{}"[..], Frame::Message(json!({}))),
        (&b"\x88\x02\x03\xf0"[..], Frame::Close(Some(1008))),
    ] {
        let (mut socket, mut peer) = socket_pair();
        let after_header = std::sync::Barrier::new(2);
        let after_close = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                peer.write_all(wire).expect("frame");
                after_header.wait();
                nix::sys::socket::setsockopt(
                    &peer,
                    nix::sys::socket::sockopt::Linger,
                    &nix::libc::linger {
                        l_onoff: 1,
                        l_linger: 0,
                    },
                )
                .expect("abortive close");
                drop(peer);
                after_close.wait();
            });
            let mut head = [0; 2];
            socket.stream.read_exact(&mut head).expect("frame header");
            after_header.wait();
            after_close.wait();
            let deadline = Instant::now() + WAIT;
            loop {
                if let Err(error) = socket.stream.set_read_timeout(Some(WAIT)) {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EINVAL));
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the reset did not end the socket"
                );
                std::thread::yield_now();
            }
            assert_eq!(socket.read_frame(head), expected);
            assert_eq!(socket.next_within(QUIET), Some(Frame::Gone));
        });
    }
}

#[test]
fn a_live_peer_decodes_frames_and_keeps_both_read_bounds() {
    let (mut socket, mut peer) = socket_pair();
    assert_eq!(socket.next_within(QUIET), None);
    assert_eq!(
        socket.stream.read_timeout().expect("header timeout"),
        Some(QUIET)
    );

    peer.write_all(b"\x81\x02{}").expect("JSON frame");
    assert_eq!(socket.next_within(QUIET), Some(Frame::Message(json!({}))));
    assert_eq!(socket.texts, ["{}"]);
    assert_eq!(
        socket.stream.read_timeout().expect("body timeout"),
        Some(WAIT)
    );

    peer.write_all(b"\x88\x02\x03\xf0").expect("close frame");
    assert_eq!(socket.next_within(QUIET), Some(Frame::Close(Some(1008))));
    peer.shutdown(std::net::Shutdown::Write).expect("peer FIN");
    assert_eq!(socket.next_within(QUIET), Some(Frame::Gone));
}

#[test]
#[should_panic(expected = "frame payload")]
fn a_truncated_frame_payload_still_fails() {
    let (mut socket, mut peer) = socket_pair();
    peer.write_all(b"\x81\x02{").expect("partial frame");
    peer.shutdown(std::net::Shutdown::Write).expect("peer FIN");
    socket.next_within(QUIET);
}

#[test]
fn a_live_peer_cannot_leave_the_frame_payload_unbounded() {
    let (mut socket, mut peer) = socket_pair();
    peer.write_all(b"\x81\x02").expect("frame header");
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        socket.next_within(QUIET);
    }));
    let panic = failed.expect_err("a withheld payload was accepted");
    let message = panic.downcast_ref::<String>().expect("the payload error");
    assert!(message.starts_with("frame payload:"), "{message}");
    assert_eq!(
        socket.stream.read_timeout().expect("body timeout"),
        Some(WAIT)
    );
    peer.write_all(b"{}").expect("the peer is still live");
}

#[test]
#[should_panic(expected = "the read timeout must be positive")]
fn a_zero_read_wait_still_fails() {
    let (socket, _peer) = socket_pair();
    socket.read_timeout(Duration::ZERO);
}

#[test]
fn grant_each_tuple_field_is_compared() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    terminal.write(&handle, b"control");
    await_write(&floor.ptys, b"control");
    floor.ptys.produce_output(pty, b"visible");
    assert_eq!(terminal.data(), b"visible");
    terminal.close();
    await_system_detach(&fixture, agent, 1);
    floor.ptys.await_native_acknowledged(pty);

    let fields = [
        SkewField::Session,
        SkewField::BrowserEpoch,
        SkewField::DaemonEpoch,
        SkewField::Agent,
        SkewField::Pty,
    ];
    for (round, field) in fields.into_iter().enumerate() {
        let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
        let handle = terminal.attach(agent);
        let pty = newest_pty(&floor.ptys);
        let before = host_bytes(&floor.ptys);
        floor.skew.arm(field);
        terminal.write(&handle, format!("skewed {field:?}").as_bytes());
        terminal.expect_terminal_closed();
        await_system_detach(&fixture, agent, round + 2);
        floor.ptys.await_native_acknowledged(pty);
        assert_eq!(
            host_bytes(&floor.ptys),
            before,
            "{field:?} reached the host"
        );
    }

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    terminal.write(&handle, b"after");
    await_write(&floor.ptys, b"after");
    floor.stop();
}

#[test]
fn grant_wrong_and_cross_connection_handles_write_nothing() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let other = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let (other_cookie, other_csrf) = session(&floor);

    let mut first = Socket::terminal(&floor, &cookie, &csrf);
    let handle = first.attach(agent);
    let mut second = Socket::terminal(&floor, &other_cookie, &other_csrf);
    let second_handle = second.attach(other);
    let before = host_bytes(&floor.ptys);

    second.write(&handle, b"another session's handle");
    second.expect_policy_close();
    await_system_detach(&fixture, other, 1);
    let mut wrong = handle.clone();
    wrong.replace_range(0..1, if wrong.starts_with('A') { "B" } else { "A" });
    first.write(&wrong, b"a wrong handle");
    first.expect_policy_close();
    await_system_detach(&fixture, agent, 1);

    let mut later = Socket::terminal(&floor, &cookie, &csrf);
    later.write(&handle, b"a closed channel's handle");
    later.expect_policy_close();
    let mut later = Socket::terminal(&floor, &other_cookie, &other_csrf);
    later.write(&second_handle, b"a closed channel's handle");
    later.expect_policy_close();
    assert_eq!(host_bytes(&floor.ptys), before);
    floor.stop();
}

#[test]
fn grant_idle_expiry_writes_nothing() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    for minute in [20, 40] {
        floor.clock.advance(20 * 60_000);
        let typed = format!("typed at minute {minute}");
        terminal.write(&handle, typed.as_bytes());
        await_write(&floor.ptys, typed.as_bytes());
    }
    let before = host_bytes(&floor.ptys);
    floor.clock.advance(31 * 60_000);
    terminal.write(&handle, b"after idle expiry");
    terminal.expect_policy_close();
    await_system_detach(&fixture, agent, 1);
    assert_eq!(host_bytes(&floor.ptys), before);
    floor.stop();
}

/// Each output frame is checked under the session lock, so output after the absolute expiry and
/// before the two-second recheck reaches the browser as no frame.
#[test]
fn grant_output_after_absolute_expiry_sends_no_frame() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    floor.clock.advance(8 * 60 * 60_000);
    floor
        .ptys
        .produce_output(newest_pty(&floor.ptys), b"after absolute expiry");
    let before_recheck = Duration::from_millis(1_500);
    match terminal.next_within(before_recheck) {
        Some(Frame::Message(message)) => {
            assert_eq!(message["type"], "terminal.closed", "{message}");
        }
        other => panic!("expected terminal.closed and no data, got {other:?}"),
    }
    assert_eq!(terminal.next(), Frame::Close(None));
    await_system_detach(&fixture, agent, 1);
    floor.stop();
}

#[test]
fn grant_a_silent_expired_channel_closes_within_two_seconds() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let opened = Instant::now();
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    floor.clock.advance(8 * 60 * 60_000);
    terminal.expect_terminal_closed();
    let silent = opened.elapsed();
    assert!(
        silent < Duration::from_secs(2),
        "a silent expired channel closed after {silent:?}"
    );
    await_system_detach(&fixture, agent, 1);
    floor.stop();
}

#[test]
fn grant_browser_close_and_pty_exit_revoke_and_close() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    terminal.close();
    await_system_detach(&fixture, agent, 1);
    floor.ptys.await_native_acknowledged(pty);
    let mut after = Socket::terminal(&floor, &cookie, &csrf);
    after.write(&handle, b"after the browser closed");
    after.expect_policy_close();

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    floor.ptys.end_output(newest_pty(&floor.ptys));
    terminal.expect_terminal_closed();
    await_system_detach(&fixture, agent, 2);
    floor.stop();
}

/// A full channel reaches the browser within the 8 MiB wire bound; one byte more closes the
/// channel instead of dropping bytes.
#[test]
fn grant_output_past_the_wire_bound_closes_the_channel() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    floor
        .ptys
        .produce_output(pty, &vec![b'w'; TERMINAL_QUEUE_BYTES]);
    let first_frame = terminal.texts.len();
    let mut received = 0;
    while received < TERMINAL_QUEUE_BYTES {
        received += terminal.data().len();
    }
    assert_eq!(received, TERMINAL_QUEUE_BYTES);
    let header = 10;
    let wire: usize = terminal.texts[first_frame..]
        .iter()
        .map(|text| text.len() + header)
        .sum();
    assert!(wire <= WIRE_BOUND_BYTES, "{wire} bytes on the wire");

    floor
        .ptys
        .produce_output(pty, &vec![b'w'; TERMINAL_QUEUE_BYTES + 1]);
    terminal.expect("terminal.closed");
    assert_eq!(terminal.next(), Frame::Close(None));
    await_system_detach(&fixture, agent, 1);
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::PtyCloseRequested { pty: seen, actor: Actor::System, provenance: None, reason }
            if *seen == pty && reason == "terminal output overflowed"
    )));
    floor.stop();
}

#[test]
fn grant_restart_leaves_no_grant() {
    let fixture = Fixture::new();
    let mut first = floor(&fixture, true);
    let agent = attachable(&first, &fixture);
    let (cookie, csrf) = session(&first);
    let mut terminal = Socket::terminal(&first, &cookie, &csrf);
    let handle = terminal.attach(agent);
    terminal.write(&handle, b"before restart");
    await_write(&first.ptys, b"before restart");
    first.stop();

    let mut second = floor(&fixture, true);
    assert!(
        Socket::open(
            second.addr,
            "/floor/terminal",
            "dudes-terminal-v1",
            &cookie,
            &csrf
        )
        .expect_err("an old cookie")
        .starts_with("HTTP/1.1 403 Forbidden")
    );
    let (cookie, csrf) = session(&second);
    let mut terminal = Socket::terminal(&second, &cookie, &csrf);
    terminal.write(&handle, b"an old run's handle");
    terminal.expect_policy_close();
    assert!(second.ptys.writes().is_empty());
    second.stop();
}

#[test]
fn grant_resync_keeps_the_terminal_and_slots_hold_recovery() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);

    let projection = Socket::projection(&floor, &cookie, &csrf);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    drop(projection);
    let resynced = Socket::projection(&floor, &cookie, &csrf);
    terminal.write(&handle, b"after resync");
    await_write(&floor.ptys, b"after resync");
    floor
        .ptys
        .produce_output(newest_pty(&floor.ptys), b"still live");
    assert_eq!(terminal.data(), b"still live");

    let overlap = Socket::projection(&floor, &cookie, &csrf);
    let refused = |path, protocol| {
        Socket::open(floor.addr, path, protocol, &cookie, &csrf)
            .expect_err("a slot past the session's three")
    };
    assert!(refused("/floor/stream", "dudes-floor-v3").starts_with("HTTP/1.1 403"));
    assert!(refused("/floor/terminal", "dudes-terminal-v1").starts_with("HTTP/1.1 403"));
    assert!(recover(&floor, &cookie).starts_with("HTTP/1.1 403"));

    drop(resynced);
    drop(overlap);
    floor.await_connections(1);
    assert!(
        recover(&floor, &cookie).starts_with("HTTP/1.1 403"),
        "the terminal alone holds recovery"
    );
    terminal.close();
    await_system_detach(&fixture, agent, 1);
    floor.await_connections(0);
    assert!(recover(&floor, &cookie).starts_with("HTTP/1.1 200"));
    floor.stop();
}

/// `reason` is the close reason the stop journals: `agent retired` or `agent halted`.
fn stop_agent(agent: AgentId, reason: &str) -> Ask {
    if reason == "agent halted" {
        Ask::Halt(agent)
    } else {
        Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "the test stops it".into(),
        }))
    }
}

#[test]
fn grant_retire_and_halt_revoke_and_refuse() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let halted = attachable(&floor, &fixture);
    let retired = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let handle = floor.running.handle.clone();

    for (agent, reason) in [(halted, "agent halted"), (retired, "agent retired")] {
        let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
        terminal.attach(agent);
        let pty = newest_pty(&floor.ptys);
        let answer = handle.ask(stop_agent(agent, reason));
        assert!(matches!(answer, Ok(Answered::Done)), "{answer:?}");
        terminal.expect_terminal_closed();
        await_system_detach(&fixture, agent, 1);
        let entries = fixture.entries();
        let request = entries
            .iter()
            .find_map(|(lsn, entry)| match &entry.record {
                Record::PtyCloseRequested {
                    pty: seen,
                    actor: Actor::System,
                    reason: why,
                    ..
                } if *seen == pty && why == reason => Some(*lsn),
                _ => None,
            })
            .expect("the system close request");
        let completed = entries
            .iter()
            .find_map(|(lsn, entry)| match &entry.record {
                Record::PtyCloseCompleted {
                    request_lsn,
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                } if *request_lsn == request => Some(*lsn),
                _ => None,
            })
            .expect("the confirmed close");
        let detached = entries
            .iter()
            .find_map(|(lsn, entry)| match &entry.record {
                Record::Detached {
                    agent: seen,
                    by: Actor::System,
                    ..
                } if *seen == agent => Some(*lsn),
                _ => None,
            })
            .expect("the system detach");
        assert!(request < completed && completed < detached);

        let opens = floor.ptys.opens().len();
        let attached = count(&fixture, |record| matches!(record, Record::Attached { .. }));
        let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
        terminal.send(&json!({"type": "terminal.attach", "agent": agent, "cols": 80, "rows": 24}));
        terminal.expect_terminal_closed();
        assert_eq!(
            floor.ptys.opens().len(),
            opens,
            "{reason}: a refused attach opened a PTY"
        );
        assert_eq!(
            count(&fixture, |record| matches!(record, Record::Attached { .. })),
            attached,
            "{reason}: a refused attach was journaled"
        );
    }
    floor.stop();
}

#[test]
fn an_unresolved_append_fences_every_browser_terminal_effect() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let bound = attachable(&floor, &fixture);
    let other = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(bound);
    let owned = floor.ptys.live();
    assert_eq!(owned.len(), 1);

    floor
        .post_fsync_failure
        .arm(OnboardingFailurePoint::LocalRetire);
    let retire = floor.running.handle.ask(Ask::Retire(Box::new(RetireParams {
        agent: other,
        reason: "durable but not folded".into(),
    })));
    assert!(retire.is_err(), "the retirement's fold failed: {retire:?}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentRetired { id, .. } if *id == other
    )));
    terminal.expect_terminal_closed();
    let frozen = fixture.records();
    assert_eq!(floor.ptys.live(), owned);

    let opens = floor.ptys.opens().len();
    for agent in [other, bound] {
        let mut late = Socket::terminal(&floor, &cookie, &csrf);
        late.send(&json!({"type": "terminal.attach", "agent": agent, "cols": 80, "rows": 24}));
        late.expect_terminal_closed();
    }
    assert_eq!(
        floor.ptys.opens().len(),
        opens,
        "a fenced attach opened a PTY"
    );
    assert_eq!(fixture.records(), frozen);
    assert_eq!(floor.ptys.live(), owned);
    floor.stop();
    assert_eq!(floor.ptys.shutdowns(), 1);
    assert!(floor.ptys.live().is_empty());
    assert_eq!(fixture.records(), frozen);
}

#[test]
fn a_retire_or_halt_revokes_the_grant_before_its_append() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let (cookie, csrf) = session(&floor);
    for (verb, reason) in [("retire", "agent retired"), ("halt", "agent halted")] {
        let agent = attachable(&floor, &fixture);
        let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
        let handle = terminal.attach(agent);
        let pty = newest_pty(&floor.ptys);

        floor.append_failure.fail_nth(std::num::NonZeroUsize::MIN);
        let answer = floor.running.handle.ask(stop_agent(agent, reason));
        assert!(answer.is_err(), "the {verb} append failed: {answer:?}");
        assert!(!fixture.records().iter().any(|record| matches!(
            record,
            Record::AgentRetired { id, .. } | Record::Halt { agent: id, .. } if *id == agent
        )));
        let before = host_bytes(&floor.ptys);
        terminal.write(&handle, format!("after a failed {verb}").as_bytes());
        terminal.expect_any_close();
        await_system_detach(&fixture, agent, 1);
        assert_eq!(
            host_bytes(&floor.ptys),
            before,
            "{verb}: the write reached the host"
        );
        assert!(fixture.records().iter().any(|record| matches!(
            record,
            Record::PtyCloseRequested { pty: seen, reason: why, .. }
                if *seen == pty && why == reason
        )));
    }
    floor.stop();
}

#[test]
fn a_browser_write_queued_during_shutdown_reaches_no_pty() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agents = [
        attachable(&floor, &fixture),
        attachable(&floor, &fixture),
        attachable(&floor, &fixture),
    ];
    let mut channels = Vec::new();
    for agent in agents {
        let (cookie, csrf) = session(&floor);
        let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
        let handle = terminal.attach(agent);
        channels.push((terminal, handle));
    }
    let [blocked, retired, bystander] = &mut channels[..] else {
        unreachable!("three channels")
    };

    let held = floor.ptys.block_host_calls();
    blocked.0.write(&blocked.1, b"holds the loop in the host");
    std::thread::sleep(QUIET);
    floor.running.handle.stop(duded::ExitCause::Ended);
    let retire = floor
        .running
        .handle
        .ask_queued(Ask::Retire(Box::new(RetireParams {
            agent: agents[1],
            reason: "queued during shutdown".into(),
        })))
        .expect("retire queued");
    retired.0.write(&retired.1, b"after the retirement");
    bystander.0.write(&bystander.1, b"after the stop");
    std::thread::sleep(QUIET);
    drop(held);

    let _ = retire.recv_timeout(WAIT);
    floor.stop();
    let written = host_bytes(&floor.ptys);
    assert!(written.contains(&b"holds the loop in the host".to_vec()));
    for late in [&b"after the retirement"[..], &b"after the stop"[..]] {
        assert!(
            !written.iter().any(|bytes| bytes == late),
            "{:?} reached a PTY during shutdown",
            String::from_utf8_lossy(late)
        );
    }
}

#[test]
fn a_shutdown_revokes_every_grant_before_its_ptys_close() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    let sink = floor.ptys.output_sink(pty);

    let held = floor.ptys.block_host_calls();
    floor.running.handle.stop(duded::ExitCause::Ended);
    fixture.await_record("the shutdown's PTY close", |record| {
        matches!(record, Record::PtyCloseRequested { pty: seen, .. } if *seen == pty).then_some(())
    });
    sink.push(b"output after the shutdown began");
    loop {
        match terminal.next_within(QUIET) {
            None | Some(Frame::Close(_) | Frame::Gone) => break,
            Some(Frame::Message(message)) => assert_ne!(
                message["type"], "terminal.data",
                "output reached the browser after the shutdown began"
            ),
        }
    }
    drop(held);
    drop(sink);
    floor.stop();
}

#[test]
fn browser_terminal_never_uses_operator_rpc() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let digest: [u8; 32] = Sha256::digest(cookie.as_bytes()).into();
    let browser = Actor::BrowserSession {
        session_digest: dudes_proto::record::Hex32::new(digest),
    };

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    let loop_handle = floor.running.handle.clone();
    let _ = loop_handle.ask(Ask::Ask {
        agent,
        text: "PLANTED-QUESTION-BODY".into(),
    });
    let _ = loop_handle.ask(Ask::Steer {
        agent,
        text: "PLANTED-STEER-BODY".into(),
        provenance: PeerProvenance::NoRecordedAgent,
    });
    let _ = loop_handle.ask(Ask::Answer {
        agent,
        text: "PLANTED-ANSWER-BODY".into(),
        provenance: PeerProvenance::NoRecordedAgent,
    });
    terminal.write(&handle, b"typed by the browser");
    await_write(&floor.ptys, b"typed by the browser");
    assert_eq!(
        host_bytes(&floor.ptys),
        vec![b"typed by the browser".to_vec()],
        "only the browser's explicit bytes reach the PTY"
    );

    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Attached { agent: seen, by, provenance: None, .. } if *seen == agent && *by == browser
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::PtyWriteRequested { actor, provenance: None, .. } if *actor == browser
    )));
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::PtyWriteRequested {
                actor: Actor::Peer { .. },
                ..
            } | Record::ConditionedEffectSuppressed { .. }
        )),
        "the browser write went through the local router"
    );

    assert_finite_wire_admits_no_other_action(&floor, &fixture, agent);

    terminal.send(&json!({"type": "terminal.detach", "handle": handle}));
    terminal.expect("terminal.detached");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::Detached { agent: seen, by, provenance: None } if *seen == agent && *by == browser
    )));

    let credential = credential(&floor.settings);
    assert!(
        terminal
            .texts
            .iter()
            .all(|text| !text.contains(credential.trim())),
        "a frame carried the operator credential"
    );
    assert_no_record_holds(&fixture, &cookie, &handle);
    assert_decode_ties_provenance_to_actor(&fixture, &browser);
    floor.stop();
    let mut replayed = self::floor(&fixture, true);
    replayed.stop();
}

fn assert_finite_wire_admits_no_other_action(floor: &Floor, fixture: &Fixture, agent: AgentId) {
    let (cookie, csrf) = session(floor);
    for kind in ["hire", "goal", "answer", "pause", "resume"] {
        let mut probe = Socket::terminal(floor, &cookie, &csrf);
        let before = fixture.records().len();
        probe.send(&json!({"type": format!("terminal.{kind}"), "agent": agent}));
        probe.expect_policy_close();
        assert_eq!(fixture.records().len(), before, "{kind} appended a record");
    }
}

fn assert_no_record_holds(fixture: &Fixture, cookie: &str, handle: &str) {
    let mut journal = Vec::new();
    for entry in std::fs::read_dir(fixture.journal_dir()).expect("journal dir") {
        let path = entry.expect("journal entry").path();
        if path.is_file() {
            journal.extend(std::fs::read(path).expect("journal file"));
        }
    }
    let contains = |haystack: &[u8], needle: &str| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    let planted = [journal.as_slice(), cookie.as_bytes()].concat();
    assert!(
        contains(&planted, cookie),
        "the search finds a planted cookie"
    );
    assert!(!contains(&journal, cookie), "a record holds the cookie");
    assert!(!contains(&journal, handle), "a record holds the handle");
}

fn assert_decode_ties_provenance_to_actor(fixture: &Fixture, browser: &Actor) {
    let line = fixture
        .entries()
        .into_iter()
        .find(|(_, entry)| matches!(entry.record, Record::PtyWriteRequested { .. }))
        .map(|(_, entry)| serde_json::to_string(&entry).expect("encode"))
        .expect("the browser write");
    assert!(serde_json::from_str::<Entry>(&line).is_ok());
    let with_provenance = line.replace(
        r#""payload""#,
        r#""provenance":{"kind":"no_recorded_agent"},"payload""#,
    );
    assert!(serde_json::from_str::<Entry>(&with_provenance).is_err());
    let peer_without = line.replace(
        &serde_json::to_string(browser).expect("actor"),
        r#"{"kind":"peer","pid":7}"#,
    );
    assert!(serde_json::from_str::<Entry>(&peer_without).is_err());
}

#[test]
fn browser_resize_reaches_only_its_pty_and_a_bad_size_is_a_policy_close() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    terminal.send(&json!({"type": "terminal.resize", "handle": handle, "cols": 132, "rows": 50}));
    terminal.write(&handle, b"ordered after the resize");
    await_write(&floor.ptys, b"ordered after the resize");
    assert_eq!(floor.ptys.resizes(), vec![(pty, 132, 50)]);

    floor.ptys.fail_next_resize();
    terminal.send(&json!({"type": "terminal.resize", "handle": handle, "cols": 90, "rows": 30}));
    terminal.expect_terminal_closed();
    assert!(
        terminal.texts.iter().all(|text| !text.contains("injected")),
        "the host's error reached the browser"
    );
    await_system_detach(&fixture, agent, 1);
    floor.ptys.await_native_acknowledged(pty);

    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    let handle = terminal.attach(agent);
    terminal.send(&json!({"type": "terminal.resize", "handle": handle, "cols": 0, "rows": 50}));
    terminal.expect_policy_close();
    assert_eq!(floor.ptys.resizes().len(), 2, "a bad size reached the host");
    floor.stop();
}

#[test]
fn detach_purges_old_output_before_the_boundary_and_a_new_grant_sees_only_its_pty() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);

    let first = terminal.attach(agent);
    let old = newest_pty(&floor.ptys);
    floor.ptys.produce_output(old, b"old screen");
    assert_eq!(terminal.data(), b"old screen");
    floor.ptys.produce_output(old, b"late old output");
    terminal.send(&json!({"type": "terminal.detach", "handle": first}));
    loop {
        match terminal.next() {
            Frame::Message(message) if message["type"] == "terminal.detached" => break,
            Frame::Message(message) if message["type"] == "terminal.data" => {
                let data = message["data"].as_str().expect("data");
                assert_eq!(
                    decode_terminal_input(data).expect("canonical").0,
                    b"late old output"
                );
            }
            other => panic!("expected terminal.detached, got {other:?}"),
        }
    }
    assert!(
        terminal.next_within(QUIET).is_none(),
        "no frame follows terminal.detached until the next attach"
    );
    floor.ptys.await_native_acknowledged(old);

    let second = terminal.attach(agent);
    assert_ne!(first, second, "a new attach mints a new handle");
    let new = newest_pty(&floor.ptys);
    assert_ne!(old, new);
    floor.ptys.produce_output(new, b"new screen");
    assert_eq!(terminal.data(), b"new screen");
    let carrying: Vec<&String> = terminal
        .texts
        .iter()
        .filter(|text| text.contains(&first) || text.contains(&second))
        .collect();
    assert_eq!(
        carrying.len(),
        2,
        "the handle appears only in terminal.attached"
    );
    assert!(
        carrying
            .iter()
            .all(|text| text.contains("terminal.attached"))
    );
    floor.stop();
}

#[test]
fn terminal_csp_keeps_one_origin_and_exact_routes() {
    let fixture = Fixture::new();
    let mut disabled = floor(&fixture, false);
    assert_eq!(
        index_csp(&disabled),
        CSP,
        "a disabled daemon keeps the default bytes"
    );
    disabled.stop();

    let fixture = Fixture::new();
    let mut floor = floor(&fixture, true);
    let (cookie, csrf) = session(&floor);
    let csp = index_csp(&floor);
    assert_eq!(csp, TERMINAL_CSP);
    let differing: Vec<(&str, &str)> = csp
        .split("; ")
        .zip(CSP.split("; "))
        .filter(|(enabled, default)| enabled != default)
        .collect();
    assert_eq!(
        differing,
        [("style-src 'self' 'unsafe-inline'", "style-src 'self'")],
        "only style-src differs from the default"
    );
    let connect: Vec<&str> = csp
        .split(';')
        .map(str::trim)
        .find_map(|directive| directive.strip_prefix("connect-src "))
        .expect("connect-src")
        .split_whitespace()
        .collect();
    assert_eq!(
        connect,
        ["'self'"],
        "connect-src names only the page's own origin"
    );

    drop(Socket::terminal(&floor, &cookie, &csrf));
    floor.await_connections(0);
    for alias in [
        "/floor/terminal/",
        "/floor/terminal?x=1",
        "/floor//terminal",
        "/floor/Terminal",
        "/floor/terminal/../terminal",
        "/terminal",
    ] {
        let head = Socket::open(floor.addr, alias, "dudes-terminal-v1", &cookie, &csrf)
            .expect_err("an alias does not upgrade");
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{alias}: {head}"
        );
        assert!(
            head.contains(&format!("\r\nContent-Security-Policy: {TERMINAL_CSP}\r\n")),
            "{alias}: a refusal carries the run's one CSP: {head}"
        );
    }
    floor.stop();
}

fn index_csp(floor: &Floor) -> String {
    let host = floor.addr.to_string();
    let index = String::from_utf8(raw_response(
        floor.addr,
        &format!("GET /floor/ HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    ))
    .expect("ASCII head");
    index
        .lines()
        .find_map(|line| line.strip_prefix("Content-Security-Policy: "))
        .expect("a CSP header")
        .to_owned()
}

#[test]
fn browser_terminal_is_absent_when_not_enabled() {
    let fixture = Fixture::new();
    let mut disabled = floor(&fixture, false);
    let (cookie, csrf) = session(&disabled);
    let snapshot = snapshot_of(&disabled, &cookie, &csrf);
    let request = |path| {
        raw_response(
            disabled.addr,
            &upgrade(disabled.addr, path, "dudes-terminal-v1", &cookie, &csrf),
        )
    };
    let terminal = request("/floor/terminal");
    assert!(
        String::from_utf8_lossy(&terminal).starts_with("HTTP/1.1 400 Bad Request"),
        "{}",
        String::from_utf8_lossy(&terminal)
    );
    assert_eq!(
        terminal,
        request("/floor/terminal-unrouted"),
        "disabled is byte-equal to unrouted"
    );
    assert!(snapshot.get("terminal").is_none(), "{snapshot}");
    assert!(disabled.ptys.opens().is_empty());
    disabled.stop();

    let fixture = Fixture::new();
    let mut enabled = floor(&fixture, true);
    let (cookie, csrf) = session(&enabled);
    assert_eq!(snapshot_of(&enabled, &cookie, &csrf)["terminal"], json!({}));
    drop(Socket::terminal(&enabled, &cookie, &csrf));
    enabled.stop();

    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let load = |text: &str| {
        std::fs::write(&config, text).expect("operator config");
        duded::config::load_operator(&config).map(|config| config.terminal_enabled)
    };
    assert!(!load("").expect("absent is off"));
    assert!(!load("[floor.terminal]\nenabled = false\n").expect("off"));
    assert!(load("[floor.terminal]\nenabled = true\n").expect("on"));
    assert!(
        load("[floor.terminal]\nenabled = true\nshell = true\n").is_err(),
        "an unknown key is refused"
    );
}

fn snapshot_of(floor: &Floor, cookie: &str, csrf: &str) -> Value {
    let mut socket = Socket::open_retrying(floor, "/floor/stream", "dudes-floor-v3", cookie, csrf);
    socket.send(&json!({"type": "floor.subscribe"}));
    socket.expect("floor.snapshot")
}

const RETIRE_ON: FloorControl = FloorControl::On {
    goal: false,
    retire: true,
    renew: false,
};

fn retire(floor: &Floor, (cookie, csrf): (&str, &str), k: u8, agent: AgentId) -> (String, Value) {
    let request_id = format!("{}{}", char::from(b'A' + k), "A".repeat(42));
    common::floor_post(
        floor.addr,
        "/floor/retire",
        (cookie, csrf),
        &json!({ "request_id": request_id, "agent": agent }),
    )
}

/// The browser arm of `browser_retire_closes_an_attached_terminal`: an agent attached
/// through one Floor session's grant is retired from another session. The grant is revoked in the
/// retirement's job: output the PTY produces after the `200` never reaches the channel, which
/// sends `terminal.closed` and then its close frame with no `terminal.data` between or after, the
/// PTY host closes the PTY, and the journal holds the system close in a fixed order.
///
/// The PTY host is held for the retirement, so the close it queues cannot end the output first.
/// Mutant: the output pump skips its grant check, and the post-retirement output reaches the page.
#[test]
fn browser_retire_closes_an_attached_terminal() {
    let fixture = Fixture::new();
    let mut floor = floor_with(&fixture, true, RETIRE_ON);
    let agent = attachable(&floor, &fixture);
    let (holder_cookie, holder_csrf) = session(&floor);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &holder_cookie, &holder_csrf);
    terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);
    let sink = floor.ptys.output_sink(pty);
    sink.push(b"before the retirement");
    assert_eq!(terminal.data(), b"before the retirement");

    let host_lock = floor.ptys.block_host_calls();
    let (head, response) = retire(&floor, (&cookie, &csrf), 1, agent);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    sink.push(b"after the retirement");
    let mut data = Vec::new();
    loop {
        match terminal.next() {
            Frame::Message(message) if message["type"] == "terminal.data" => data.push(message),
            Frame::Message(message) if message["type"] == "terminal.closed" => break,
            other => panic!("expected terminal.closed, got {other:?}"),
        }
    }
    assert_eq!(
        terminal.next(),
        Frame::Close(None),
        "nothing follows terminal.closed"
    );
    assert!(data.is_empty(), "output after the revocation: {data:?}");
    drop(host_lock);

    await_system_detach(&fixture, agent, 1);
    assert!(
        floor.ptys.closes().contains(&pty),
        "the host closed the PTY"
    );
    let entries = fixture.entries();
    let position = |select: &dyn Fn(&Record) -> bool| {
        entries
            .iter()
            .find_map(|(lsn, entry)| select(&entry.record).then_some(*lsn))
            .expect("the record")
    };
    let retired = position(
        &|record| matches!(record, Record::AgentRetired { id, browser_retire: Some(_), .. } if *id == agent),
    );
    let request = position(&|record| {
        matches!(record, Record::PtyCloseRequested { pty: seen, actor: Actor::System, reason, .. }
            if *seen == pty && reason == "agent retired")
    });
    let completed = position(&|record| {
        matches!(record, Record::PtyCloseCompleted {
            request_lsn,
            outcome: dudes_proto::PtyCloseOutcome::Closed,
        } if *request_lsn == request)
    });
    let detached = position(
        &|record| matches!(record, Record::Detached { agent: seen, by: Actor::System, .. } if *seen == agent),
    );
    assert!(retired < request && request < completed && completed < detached);
    floor.stop();
}

/// The `browser_retire_lost_permit_closes_nothing`. A session revoked after the world
/// check and before its append permit gets the uniform empty `403`: no `AgentRetired`, no
/// `PtyCloseRequested`, and another session's grant on the target stays live and still writes.
/// In the opposite order, revocation waits for the held permit's fsync and the request gets `200`.
///
/// Mutants: the writer's permit match omits an attributed `AgentRetired`, and the revoked request
/// gets `200`; or the job revokes the terminal before the append, and a close request is journaled.
#[test]
fn browser_retire_lost_permit_closes_nothing() {
    let fixture = Fixture::new();
    let mut floor = floor_with(&fixture, true, RETIRE_ON);
    let agent = attachable(&floor, &fixture);
    let (holder_cookie, holder_csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &holder_cookie, &holder_csrf);
    let handle = terminal.attach(agent);

    let (cookie, csrf) = session(&floor);
    floor.pause.arm(OnboardingPausePoint::BeforeRetirePermit);
    let losing = {
        let (addr, cookie, csrf) = (floor.addr, cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            common::floor_post(
                addr,
                "/floor/retire",
                (&cookie, &csrf),
                &json!({ "request_id": "B".to_owned() + &"A".repeat(42), "agent": agent }),
            )
        })
    };
    assert!(
        floor.pause.wait_reached(WAIT),
        "admitted, world-checked, before the permit"
    );
    assert!(
        floor.sessions.revoke_for_test(&cookie),
        "the session is revoked"
    );
    floor.pause.release();
    let (head, response) = losing.join().expect("the revoked request");
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "{head} {response}"
    );
    assert!(response.is_null(), "{response}");
    assert_eq!(
        count(&fixture, |record| matches!(
            record,
            Record::AgentRetired { .. }
        )),
        0
    );
    assert_eq!(
        count(&fixture, |record| matches!(
            record,
            Record::PtyCloseRequested { .. }
        )),
        0,
        "the lost request closed nothing"
    );
    terminal.write(&handle, b"still attached");
    await_write(&floor.ptys, b"still attached");

    let (cookie, csrf) = session(&floor);
    floor.pause.arm(OnboardingPausePoint::AfterRetirePermit);
    let owner = {
        let (addr, cookie, csrf) = (floor.addr, cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            common::floor_post(
                addr,
                "/floor/retire",
                (&cookie, &csrf),
                &json!({ "request_id": "C".to_owned() + &"A".repeat(42), "agent": agent }),
            )
        })
    };
    assert!(
        floor.pause.wait_reached(WAIT),
        "the append holds its permit"
    );
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let revoke = floor.sessions.clone();
    let revoker = std::thread::spawn(move || {
        done_tx
            .send(revoke.revoke_for_test(&cookie))
            .expect("revoke result");
    });
    assert!(
        done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "revocation waits while the append permit is held"
    );
    floor.pause.release();
    assert!(done_rx.recv_timeout(WAIT).expect("revocation completed"));
    revoker.join().expect("revoker thread");
    let (head, response) = owner.join().expect("the winning request");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    let _ = csrf;
    terminal.expect_terminal_closed();
    await_system_detach(&fixture, agent, 1);
    floor.stop();
}

/// Turning the terminal off in the operator file, then reloading, revokes a live grant and closes
/// its socket with no restart. The terminal is then an unrouted path, and turning it back on
/// needs a restart.
#[test]
fn turning_the_terminal_off_revokes_a_live_grant() {
    const HIRE: &str = "[floor.hire]\nenabled = true\n";
    const ON: &str = "[floor.hire]\nenabled = true\n[floor.terminal]\nenabled = true\n";
    let fixture = Fixture::new();
    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    std::fs::write(&config, ON).expect("operator config");
    let mut floor = floor_reading(&fixture, true, FloorControl::Off, Some(config.clone()));
    let agent = attachable(&floor, &fixture);
    let (cookie, csrf) = session(&floor);
    let mut terminal = Socket::terminal(&floor, &cookie, &csrf);
    terminal.attach(agent);
    let pty = newest_pty(&floor.ptys);

    let reload = |text: &str| {
        std::fs::write(&config, text).expect("operator config");
        let sha256 = Sha256::digest(text.as_bytes())
            .iter()
            .fold(String::new(), |hex, byte| format!("{hex}{byte:02x}"));
        let mut client = Client::connect(floor.settings.socket()).expect("daemon socket");
        let reply = client
            .call_as_operator(
                Method::FloorEnrollmentReload,
                Some(json!({"apply": {
                    "sha256": sha256, "added": [], "removed": [], "changed": []
                }})),
                OperatorCredential::new(credential(&floor.settings)).expect("credential"),
            )
            .expect("reload response");
        reply.result.expect("a reload result")
    };
    let result = reload(HIRE);
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(result["switches"], json!(["floor.terminal"]), "{result}");

    terminal.expect_terminal_closed();
    await_system_detach(&fixture, agent, 1);
    assert!(
        floor.ptys.closes().contains(&pty),
        "the host closed the PTY"
    );
    let refused = raw_response(
        floor.addr,
        &upgrade(
            floor.addr,
            "/floor/terminal",
            "dudes-terminal-v1",
            &cookie,
            &csrf,
        ),
    );
    assert!(
        String::from_utf8_lossy(&refused).starts_with("HTTP/1.1 400 Bad Request"),
        "{}",
        String::from_utf8_lossy(&refused)
    );

    let result = reload(ON);
    assert!(
        result["not_applied"]
            .as_array()
            .is_some_and(|names| names.contains(&json!("floor.terminal"))),
        "{result}"
    );
    assert_eq!(result["switches"], json!([]), "{result}");
    floor.stop();
}
