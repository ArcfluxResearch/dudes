//! Production-path acceptance for the authenticated browser Floor stream.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use duded::floor_stream::FloorStreamFault;
use duded::floor_stream::MAX_QUEUE_BYTES;
use duded::ports::{Answered, Ask};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::agent::RetireParams;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_rpc::Client;
use serde_json::Value;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, hire_params, seams};

/// How long a closed connection has to finish closing before the oracle refuses it.
const CLOSE_BOUND: Duration = Duration::from_secs(2);

/// The extension offer a native browser appends to its own opening handshake.
const NATIVE_EXTENSION_OFFER: &str = "permessage-deflate; client_max_window_bits";

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    listener.local_addr().expect("reserved address").port()
}

fn settings(fixture: &Fixture, port: u16) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(port),
    });
    settings
}

fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted an operator credential");
    OperatorCredential::new(text).expect("the credential is well formed")
}

fn issue(settings: &duded::Settings) -> SessionIssueResult {
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let response = client
        .call_as_operator(Method::FloorSessionIssue, None, credential_of(settings))
        .expect("floor.session.issue is answered");
    assert!(
        response.error.is_none(),
        "issue failed: {:?}",
        response.error
    );
    serde_json::from_value(response.result.expect("issue result")).expect("typed issue result")
}

fn exchange(addr: SocketAddr, bootstrap: &str) -> (String, String) {
    let host = addr.to_string();
    let origin = format!("http://{host}");
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{bootstrap}",
        bootstrap.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("exchange request");
    let response = read_http_response(&mut stream);
    let (head, body) = response.split_at(
        response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("complete response head")
            + 4,
    );
    let head = std::str::from_utf8(head).expect("ASCII response head");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("session cookie")
        .to_owned();
    let json: Value = serde_json::from_slice(body).expect("session JSON");
    let csrf = json["csrf"].as_str().expect("CSRF nonce").to_owned();
    (cookie, csrf)
}

fn read_http_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut byte = [0_u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head byte");
        response.push(byte[0]);
    }
    let head = std::str::from_utf8(&response).expect("ASCII response head");
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let start = response.len();
    response.resize(start + length, 0);
    stream
        .read_exact(&mut response[start..])
        .expect("response body");
    response
}

struct WebSocket {
    stream: TcpStream,
}

impl WebSocket {
    fn connect(addr: SocketAddr, cookie: &str, csrf: &str) -> Self {
        let protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");
        let (stream, head) = raw_upgrade(addr, Some(cookie), Some(&protocols), None);
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{head:?}"
        );
        assert!(head.contains("Sec-WebSocket-Protocol: dudes-floor-v3\r\n"));
        assert!(
            !head
                .to_ascii_lowercase()
                .contains("sec-websocket-extensions"),
            "the native offer is accepted but never negotiated: {head:?}"
        );
        Self { stream }
    }

    /// Wait longer than the default two seconds, for a test whose subject arrives on a period.
    fn read_timeout(&self, bound: Duration) {
        self.stream
            .set_read_timeout(Some(bound))
            .expect("a read bound on the browser socket");
    }

    /// The next server message, or `None` if the read bound passed with nothing on the socket.
    ///
    /// [`Self::json`] panics on a timeout, which is right for a test expecting a frame now and
    /// wrong for one waiting out a period: "server frame head" names the read, not the thing that
    /// did not happen. A test that can be told "nothing arrived" says so itself.
    fn try_json(&mut self) -> Option<Value> {
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
            Err(error) => panic!("server frame head: {error}"),
        }
        assert_eq!(head[0] & 0x0f, 1, "server emits text or close only");
        let len = usize::from(head[1] & 0x7f);
        assert!(len <= 125, "a heartbeat-sized frame is never extended");
        let mut payload = vec![0_u8; len];
        self.stream
            .read_exact(&mut payload)
            .expect("server frame body");
        Some(serde_json::from_slice(&payload).expect("server JSON"))
    }

    fn send(&mut self, value: &str) {
        self.stream
            .write_all(&masked_text(value.as_bytes()))
            .expect("browser text frame");
    }

    fn json(&mut self) -> Value {
        let payload = self.frame().expect("server text frame");
        serde_json::from_slice(&payload).expect("server JSON")
    }

    fn frame(&mut self) -> Option<Vec<u8>> {
        let mut head = [0_u8; 2];
        self.stream
            .read_exact(&mut head)
            .expect("server frame head");
        if head[0] & 0x0f == 8 {
            return None;
        }
        assert_eq!(head[0] & 0x0f, 1, "server emits text or close only");
        assert_eq!(head[1] & 0x80, 0, "server frames are not masked");
        let len = match head[1] & 0x7f {
            value @ 0..=125 => usize::from(value),
            126 => {
                let mut bytes = [0_u8; 2];
                self.stream
                    .read_exact(&mut bytes)
                    .expect("u16 frame length");
                usize::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0_u8; 8];
                self.stream
                    .read_exact(&mut bytes)
                    .expect("u64 frame length");
                usize::try_from(u64::from_be_bytes(bytes)).expect("test frame fits usize")
            }
            _ => unreachable!(),
        };
        let mut payload = vec![0_u8; len];
        self.stream
            .read_exact(&mut payload)
            .expect("server frame body");
        Some(payload)
    }

    /// Require a real server-owned closure, and the server's own slot back before this client goes.
    ///
    /// A read that times out or would block is an open connection, not a closed one, so it fails
    /// here. What passes is EOF or a reset, optionally preceded by exactly one complete close
    /// frame; the connection's gateway slot must then be released while the client is still held,
    /// which is what distinguishes a server that closed from one the client closed.
    fn expect_closed(&mut self, probe: &duded::floor_gateway::FloorGatewayProbe) {
        let deadline = std::time::Instant::now() + CLOSE_BOUND;
        let mut closing = false;
        loop {
            let mut first = [0_u8; 1];
            match self.stream.read(&mut first) {
                Ok(0) => break,
                Ok(_) => {
                    assert!(
                        !closing,
                        "the server wrote {:#04x} after its close",
                        first[0]
                    );
                    assert_eq!(first[0], 0x88, "expected one final close frame");
                    let mut second = [0_u8; 1];
                    self.stream
                        .read_exact(&mut second)
                        .expect("close frame length");
                    assert_eq!(second[0] & 0x80, 0, "server frames are not masked");
                    let len = usize::from(second[0] & 0x7f);
                    assert!(len <= 125, "a close frame carries at most 125 bytes");
                    let mut payload = vec![0_u8; len];
                    self.stream
                        .read_exact(&mut payload)
                        .expect("close frame payload");
                    closing = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::NotConnected
                    ) =>
                {
                    break;
                }
                Err(error)
                    if closing
                        && std::time::Instant::now() < deadline
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                Err(error) => panic!(
                    "the connection is still open: {:?} does not close it",
                    error.kind()
                ),
            }
        }
        wait_for_live(probe, 0);
    }
}

fn raw_upgrade(
    addr: SocketAddr,
    cookie: Option<&str>,
    protocols: Option<&str>,
    origin: Option<&str>,
) -> (TcpStream, String) {
    raw_upgrade_offering(addr, cookie, protocols, origin, &[NATIVE_EXTENSION_OFFER])
}

/// The same upgrade with an exact list of `Sec-WebSocket-Extensions` offer headers.
fn raw_upgrade_offering(
    addr: SocketAddr,
    cookie: Option<&str>,
    protocols: Option<&str>,
    origin: Option<&str>,
    extensions: &[&str],
) -> (TcpStream, String) {
    let host = addr.to_string();
    let default_origin = format!("http://{host}");
    let mut stream = TcpStream::connect(addr).expect("connect WebSocket");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        origin.unwrap_or(&default_origin)
    );
    for offer in extensions {
        request.push_str("Sec-WebSocket-Extensions: ");
        request.push_str(offer);
        request.push_str("\r\n");
    }
    if let Some(protocols) = protocols {
        request.push_str("Sec-WebSocket-Protocol: ");
        request.push_str(protocols);
        request.push_str("\r\n");
    }
    if let Some(cookie) = cookie {
        request.push_str("Cookie: dudes_floor_session=");
        request.push_str(cookie);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("upgrade request");
    let head = read_head(&mut stream).expect("upgrade response head");
    (stream, head)
}

fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8(head).expect("ASCII response head"))
}

fn wait_for_live(probe: &duded::floor_gateway::FloorGatewayProbe, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut maximum = 0;
    while probe.live_connections() != expected {
        maximum = maximum.max(probe.live_connections());
        assert!(
            std::time::Instant::now() < deadline,
            "expected {expected} live connections, got {}, maximum observed {maximum}",
            probe.live_connections(),
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pause(handle: &duded::Handle, agent: dudes_types::AgentId) {
    handle
        .ask(duded::ports::Ask::Pause(agent))
        .expect("pause reaches the daemon loop");
}

fn hire_with_role(
    handle: &duded::Handle,
    cwd: &std::path::Path,
    role: String,
) -> dudes_types::AgentId {
    let mut params = hire_params(cwd);
    params.role = role;
    match handle
        .ask(duded::ports::Ask::Hire(Box::new(params)))
        .expect("hire reaches the daemon loop")
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    }
}

fn retire(handle: &duded::Handle, agent: dudes_types::AgentId) {
    handle
        .ask(duded::ports::Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "queue-bound control".to_owned(),
        })))
        .expect("retire reaches the daemon loop");
}

fn masked_text(payload: &[u8]) -> Vec<u8> {
    let mask = [1_u8, 2, 3, 4];
    let mut frame = vec![0x81];
    match payload.len() {
        0..=125 => frame.push(0x80 | u8::try_from(payload.len()).expect("small payload")),
        126..=65_535 => {
            frame.push(0x80 | 0x7e);
            frame.extend_from_slice(
                &u16::try_from(payload.len())
                    .expect("medium payload")
                    .to_be_bytes(),
            );
        }
        _ => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(
                &u64::try_from(payload.len())
                    .expect("large payload")
                    .to_be_bytes(),
            );
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    frame
}

/// The shipped wiring beats: a booted daemon with a floor, at the shipped period, journaling
/// nothing.
///
/// **This is the test that sees what `boot` does rather than what the registry can do.** The unit
/// tests below `floor_stream.rs` drive `start_heartbeat` themselves and would stay green against a
/// daemon that never started one, or started one at the wrong period — measured by changing the code:
/// setting the period to `HEARTBEAT_PERIOD * 100_000` and separately setting `floor_heartbeat = None`
/// left the suite identical either way. So this one boots the real thing, subscribes over the
/// real socket, and waits.
///
/// Two assertions, and the second is the named control:
///
/// 1. a `{"type":"floor.heartbeat"}` frame arrives within a few of the shipped period;
/// 2. the journal is byte-for-byte the same length afterwards — a heartbeat that journaled, or that
///    rode the append path, would move it.
///
/// The clock is frozen (`ticking(.., 0)`), so nothing time-driven appends behind the assertion; the
/// timer is unaffected because it parks on a `Condvar` with a real `Duration` rather than reading
/// the daemon's clock.
#[test]
fn a_booted_floor_beats_at_the_shipped_period_and_journals_nothing() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    // The shipped fault: beats on, at `HEARTBEAT_PERIOD`, exactly as `boot` wires them.
    let fault = FloorStreamFault::default();
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
        None,
        fault.clone(),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);

    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    // Longer than the 2 000 ms period so a beat is never cut off, and well under the deadline
    // below so a silent socket is reported by this test's own assertion rather than by a read.
    browser.read_timeout(Duration::from_secs(5));
    browser.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(
        browser.json()["type"],
        "floor.snapshot",
        "a subscribe is answered with its snapshot first"
    );

    let before = fixture.entries().len();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut beats = 0_usize;
    while beats == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no floor.heartbeat arrived within 15s of subscribing to a booted floor"
        );
        let Some(message) = browser.try_json() else {
            continue;
        };
        match message["type"].as_str() {
            Some("floor.heartbeat") => {
                assert_eq!(
                    message,
                    serde_json::json!({"type": "floor.heartbeat"}),
                    "the heartbeat carried something other than its own discriminator"
                );
                beats += 1;
            }
            other => panic!("an idle floor sent {other:?} rather than a heartbeat"),
        }
    }

    assert_eq!(
        fixture.entries().len(),
        before,
        "a heartbeat appended to the journal: it is a frame, and invariant 1 has one writer"
    );
    drop(browser);
    drop(running);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the assertion is one ordered production-path transaction"
)]
fn snapshot_precedes_live_deltas() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    // The subject here is the queue's own arithmetic — "exactly 256 deltas, still live" — and
    // since a heartbeat consumes that same bound. Turning the beat off keeps this
    // test counting only its own deltas rather than widening what it asserts to absorb an arrival
    // it cannot time. That heartbeats do share the bound is asserted by
    // `floor_stream::tests::a_never_draining_subscriber_is_closed_by_heartbeats_alone`.
    let fault = FloorStreamFault::default().without_heartbeat();
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
        None,
        fault.clone(),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);

    let valid_protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");
    let old_protocols = format!("dudes-floor-v1, dudes-csrf.{csrf}");
    for (candidate_cookie, protocols, origin) in [
        (Some(cookie.as_str()), Some(old_protocols.as_str()), None),
        (None, Some(valid_protocols.as_str()), None),
        (Some(cookie.as_str()), None, None),
        (
            Some(cookie.as_str()),
            Some("dudes-floor-v3, dudes-csrf.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            None,
        ),
        (
            Some(cookie.as_str()),
            Some(valid_protocols.as_str()),
            Some("http://wrong.invalid"),
        ),
        (
            Some(cookie.as_str()),
            Some("dudes-floor-v3, dudes-floor-v3"),
            None,
        ),
        (
            Some(cookie.as_str()),
            Some("dudes-floor-v3,,dudes-csrf.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            None,
        ),
        (
            Some(cookie.as_str()),
            Some("dudes-floor-v3, dudes-csrf.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA, third"),
            None,
        ),
    ] {
        let (_stream, head) = raw_upgrade(addr, candidate_cookie, protocols, origin);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "invalid authority/carrier reached the stream: {head:?}"
        );
    }

    for offers in [
        Vec::new(),
        vec![""],
        vec![NATIVE_EXTENSION_OFFER, "permessage-deflate"],
    ] {
        let (_stream, head) = raw_upgrade_offering(
            addr,
            Some(cookie.as_str()),
            Some(valid_protocols.as_str()),
            None,
            &offers,
        );
        let expected = if offers.len() == 1 && offers[0].is_empty() || offers.len() > 1 {
            "HTTP/1.1 400 Bad Request"
        } else {
            "HTTP/1.1 101 Switching Protocols"
        };
        assert!(
            head.starts_with(expected),
            "one bounded offer header is accepted and nothing else: {offers:?} -> {head:?}"
        );
        assert!(
            !head
                .to_ascii_lowercase()
                .contains("sec-websocket-extensions"),
            "no offer is ever negotiated: {head:?}"
        );
    }
    wait_for_live(&probe, 0);

    fault.hold_next_subscription();
    let mut socket = WebSocket::connect(addr, &cookie, &csrf);
    socket.send(r#"{"type":"floor.subscribe"}"#);
    assert!(
        fault.wait_registered(Duration::from_secs(2)),
        "the production gateway registered before its snapshot write"
    );
    let before_goal = fixture.entries();
    let queued = running
        .handle
        .ask_queued(Ask::SetGoal(Box::new(goal_params(
            agent,
            &[common::true_program()],
        ))))
        .expect("local goal queued behind the held subscription");
    let answer = queued
        .recv_timeout(Duration::from_secs(2))
        .expect("the writer continues while a subscriber holds its snapshot")
        .expect("local goal admitted");
    let Answered::GoalSet(result) = answer else {
        panic!("local goal answer")
    };
    let goal = result.goal;
    assert!(
        fixture.entries().len() > before_goal.len(),
        "positive control: the live daemon journal moved"
    );
    let queued = fault.queue_usage().expect("the held production queue");
    assert!(queued.0 > 0 && queued.1 > 0 && !queued.2, "{queued:?}");
    fault.release();

    let snapshot = socket.json();
    assert_eq!(snapshot["type"], "floor.snapshot");
    assert_eq!(
        snapshot["projection"]["agents"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(snapshot["projection"]["agents"][0]["goal"].is_null());
    let snapshot_lsn = snapshot["projection"]["lsn"]
        .as_u64()
        .expect("snapshot lsn");

    let delta = socket.json();
    assert_eq!(delta["type"], "floor.delta");
    assert_eq!(delta["delta"]["agents"].as_array().map(Vec::len), Some(1));
    assert_eq!(delta["delta"]["agents"][0]["goal"]["id"], goal.to_string());
    assert_eq!(delta["delta"]["agents"][0]["role"], "infra");
    let delta_lsn = delta["delta"]["lsn"].as_u64().expect("delta lsn");
    assert!(
        delta_lsn > snapshot_lsn + 1,
        "actual journal positions jump numerically: {snapshot_lsn} -> {delta_lsn}"
    );
    assert!(duded::floor_stream::server_type_is_listed(
        delta["type"].as_str().expect("server type")
    ));

    socket.send(r#"{"type":"floor.unsubscribe"}"#);
    while let Some(payload) = socket.frame() {
        let message: Value = serde_json::from_slice(&payload).expect("queued server JSON");
        assert!(
            duded::floor_stream::server_type_is_listed(
                message["type"].as_str().expect("queued server type")
            ),
            "only accepted deltas may precede the unsubscribe close: {message}"
        );
    }

    wait_for_live(&probe, 0);
    closing_participant_spawned(&fixture, goal);
    running.handle.doctor().unwrap();
    let before_unlisted = fixture.entries();
    let mut unlisted = WebSocket::connect(addr, &cookie, &csrf);
    unlisted.send(r#"{"type":"floor.write"}"#);
    unlisted.expect_closed(&probe);
    drop(unlisted);
    wait_for_live(&probe, 0);
    assert_eq!(
        fixture.entries(),
        before_unlisted,
        "an unlisted browser type has no domain dispatch target"
    );

    fault.hold_next_subscription();
    let mut overflowed = WebSocket::connect(addr, &cookie, &csrf);
    overflowed.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));
    for _ in 0..256 {
        pause(&running.handle, agent);
    }
    assert_eq!(
        fault.queue_usage().map(|usage| (usage.0, usage.2)),
        Some((256, false)),
        "the exact message bound stays live"
    );
    pause(&running.handle, agent);
    assert_eq!(
        fault.queue_usage().map(|usage| usage.2),
        Some(true),
        "message 257 makes the epoch terminal"
    );
    fault.release();
    assert_eq!(overflowed.json()["type"], "floor.snapshot");
    overflowed.expect_closed(&probe);
    drop(overflowed);
    wait_for_live(&probe, 0);

    let mut fresh = WebSocket::connect(addr, &cookie, &csrf);
    fresh.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(
        fresh.json()["type"],
        "floor.snapshot",
        "overflow recovery begins with a fresh authenticated snapshot"
    );
    fresh.send(r#"{"type":"floor.unsubscribe"}"#);
    fresh.expect_closed(&probe);
    drop(fresh);
    wait_for_live(&probe, 0);
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

fn start_closing_cut(
    fixture: &Fixture,
    settings: &duded::Settings,
    column: &Arc<common::ScriptedColumn>,
) -> (common::Running, SocketAddr) {
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let parts = common::seams_with(
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        column.clone(),
    );
    host.bind_child_ends(Arc::clone(&parts.children));
    let daemon = duded::boot::boot_with_floor_faults(
        settings,
        parts,
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .unwrap();
    let addr = daemon.floor_addr().unwrap();
    (common::start(daemon), addr)
}

fn closing_participant_spawned(fixture: &Fixture, goal: dudes_types::GoalId) {
    let turn = fixture.await_record("the participant's goal issue", |record| match record {
        dudes_proto::Record::TurnIssued {
            goal: issued, turn, ..
        } if *issued == goal => Some(*turn),
        _ => None,
    });
    fixture.await_record("the participant's actual spawn", |record| {
        matches!(record, dudes_proto::Record::TurnSpawned { turn: seen, .. } if *seen == turn)
            .then_some(())
    });
}

#[test]
fn first_closing_frame_uses_the_durable_participant_cut() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let column = common::ScriptedColumn::new(dudes_proto::PeerProvenance::NoRecordedAgent);
    let (mut running, addr) = start_closing_cut(&fixture, &settings, &column);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(&running.handle, goal_params(agent, &["/usr/bin/true"]));
    closing_participant_spawned(&fixture, goal);
    let unanswered = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(&running.handle, goal_params(unanswered, &["/usr/bin/true"]));
    closing_participant_spawned(&fixture, goal);
    running.handle.doctor().unwrap();
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    browser.send(r#"{"type":"floor.subscribe"}"#);
    let before = browser.json();
    assert!(before["projection"].get("closing_progress").is_none());
    let mut client = Client::connect(settings.socket()).unwrap();
    let answer = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            credential_of(&settings),
        )
        .unwrap();
    assert!(answer.error.is_none());
    let frame = browser.json();
    assert_eq!(frame["type"], "floor.delta");
    assert_eq!(
        frame["delta"]["closing_time"],
        serde_json::json!({"kind":"announced"})
    );
    let progress = &frame["delta"]["closing_progress"];
    assert_progress_cut(&fixture, &frame, (0, 0, 2, 2));
    assert!((359_000..=360_000).contains(&progress["remaining_ms"].as_u64().unwrap()));
    assert_eq!(
        frame["delta"]["lsn"],
        answer.result.as_ref().unwrap()["announcement"]
    );
    let announcement = answer.result.as_ref().unwrap()["announcement"].clone();
    let token = agent_token(&fixture, agent);
    let ack = serde_json::json!({ "action": "ack", "lsn": announcement, "token": token.expose() });
    column.set(dudes_proto::PeerProvenance::Agent(agent));
    let mut caller = Client::connect(settings.socket()).unwrap();
    let acknowledged_reply = caller
        .call(Method::SystemClosingTime, Some(ack.clone()))
        .unwrap();
    assert!(
        acknowledged_reply.error.is_none(),
        "{:?}",
        acknowledged_reply.error
    );
    let acknowledged = next_closing_progress(&mut browser);
    assert!(acknowledged["delta"].get("closing_time").is_none());
    assert_progress_cut(&fixture, &acknowledged, (1, 0, 1, 2));
    let before_duplicate = fixture.entries();
    assert!(
        caller
            .call(Method::SystemClosingTime, Some(ack))
            .unwrap()
            .error
            .is_none()
    );
    assert_eq!(fixture.entries(), before_duplicate);
    observe_turn_end(&running.handle, &fixture, agent);
    let ended = next_closing_progress(&mut browser);
    assert!(
        ended["delta"].get("closing_time").is_none(),
        "an observed turn end is a progress-only delta"
    );
    assert_progress_cut(&fixture, &ended, (1, 0, 1, 1));
    let cut = ended["delta"]["lsn"].as_u64().unwrap();
    assert!(fixture.entries().iter().any(|(lsn, entry)| lsn.0 == cut
        && matches!(entry.record, dudes_proto::Record::TurnEnded { .. })));
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        dudes_proto::Record::ClosingTime {
            phase: dudes_proto::ClosingPhase::Complete
        }
    )));
    drop(browser);
    drop(client);
    running.stop(duded::ExitCause::ShutdownRpc);
}

fn agent_token(
    fixture: &Fixture,
    agent: dudes_types::AgentId,
) -> dudes_proto::rpc::hook::HookToken {
    fixture
        .records()
        .iter()
        .find_map(|record| match record {
            dudes_proto::Record::HookTokenMinted {
                agent: owner,
                token_hash,
            } if *owner == agent => Some(common::token_of(token_hash)),
            _ => None,
        })
        .unwrap()
}

fn observe_turn_end(handle: &duded::ports::Handle, fixture: &Fixture, agent: dudes_types::AgentId) {
    let turn = fixture
        .records()
        .iter()
        .find_map(|record| match record {
            dudes_proto::Record::TurnIssued {
                agent: owner, turn, ..
            } if *owner == agent => Some(*turn),
            _ => None,
        })
        .unwrap();
    handle.input(dudes_supervisor::Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: std::collections::BTreeMap::new(),
        outcome: dudes_proto::TurnOutcome::Completed,
        last_message: None,
    });
}

fn next_closing_progress(browser: &mut WebSocket) -> Value {
    loop {
        let frame = browser.json();
        if frame["delta"].get("closing_progress").is_some() {
            return frame;
        }
        assert!(
            frame["delta"].get("closing_time").is_none(),
            "every changed closing phase has progress"
        );
    }
}

fn assert_progress_cut(fixture: &Fixture, frame: &Value, expected: (u32, u32, u32, u32)) {
    let cut = &frame[if frame["type"] == "floor.snapshot" {
        "projection"
    } else {
        "delta"
    }];
    let progress = &cut["closing_progress"];
    assert!(
        progress.is_object(),
        "missing progress at expected durable cut: {frame}"
    );
    assert_eq!(
        progress
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "acknowledged",
            "settled",
            "owed",
            "running",
            "remaining_ms"
        ])
    );
    assert_eq!(
        (
            progress["acknowledged"].as_u64().unwrap(),
            progress["settled"].as_u64().unwrap(),
            progress["owed"].as_u64().unwrap(),
            progress["running"].as_u64().unwrap()
        ),
        (
            u64::from(expected.0),
            u64::from(expected.1),
            u64::from(expected.2),
            u64::from(expected.3)
        )
    );
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        if lsn.0 > cut["lsn"].as_u64().unwrap() {
            break;
        }
        world.apply(lsn, &entry);
    }
    let session = world.closing.as_ref().unwrap();
    assert_eq!(
        u64::from(expected.0 + expected.1 + expected.2),
        session.participants.len() as u64
    );
    assert_eq!(expected.2 == 0 && expected.3 == 0, session.ready(&world));
    let counts = session.counts(&world);
    assert_eq!(
        (
            counts.acknowledged,
            counts.settled,
            counts.owed,
            counts.running
        ),
        expected
    );
    assert!(progress["remaining_ms"].as_u64().unwrap() <= 360_000);
}

#[test]
fn an_unfolded_announcement_never_publishes_a_mismatched_closing_cut() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .unwrap();
    let addr = daemon.floor_addr().unwrap();
    let failure = daemon.onboarding_failure();
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    browser.send(r#"{"type":"floor.subscribe"}"#);
    let before = browser.json();
    failure.arm(duded::runtime::OnboardingFailurePoint::ClosingAnnounced);
    let mut client = Client::connect(settings.socket()).unwrap();
    let response = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({ "action": "announce" })),
            credential_of(&settings),
        )
        .unwrap();
    assert_eq!(response.error.unwrap().code, -32603);
    while let Some(payload) = browser.frame() {
        let frame: Value = serde_json::from_slice(&payload).unwrap();
        assert!(frame["delta"].get("closing_time").is_none());
        assert!(frame["delta"].get("closing_progress").is_none());
        assert_eq!(frame["delta"]["lsn"], before["projection"]["lsn"]);
    }
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                dudes_proto::Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Announced
                }
            ))
            .count(),
        1
    );
    assert!(
        client
            .call_as_operator(Method::AgentList, None, credential_of(&settings))
            .unwrap()
            .error
            .is_none()
    );
    running.stop(duded::ExitCause::ShutdownRpc);
}

#[test]
fn ten_quiet_seconds_send_no_progress_and_a_new_subscription_samples_fresh_time() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(&running.handle, goal_params(agent, &["/usr/bin/true"]));
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    browser.send(r#"{"type":"floor.subscribe"}"#);
    assert!(
        browser.json()["projection"]
            .get("closing_progress")
            .is_none()
    );
    let mut operator = Client::connect(settings.socket()).unwrap();
    assert!(
        operator
            .call_as_operator(
                Method::SystemClosingTime,
                Some(serde_json::json!({ "action": "announce" })),
                credential_of(&settings)
            )
            .unwrap()
            .error
            .is_none()
    );
    let first = next_closing_progress(&mut browser);
    assert_progress_cut(&fixture, &first, (0, 0, 1, 1));
    let sampled = first["delta"]["closing_progress"]["remaining_ms"]
        .as_u64()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        browser.read_timeout(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_millis(1)),
        );
        let mut byte = [0_u8; 1];
        match browser.stream.peek(&mut byte) {
            Ok(1) => {
                let next = browser.json();
                assert!(
                    next["delta"].get("closing_progress").is_none(),
                    "time alone published progress: {next}"
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            other => panic!("the owned closing stream remains live: {other:?}"),
        }
    }
    let mut fresh = WebSocket::connect(addr, &cookie, &csrf);
    fresh.send(r#"{"type":"floor.subscribe"}"#);
    let snapshot = fresh.json();
    assert_progress_cut(&fixture, &snapshot, (0, 0, 1, 1));
    assert_eq!(
        snapshot["projection"]["closing_time"],
        serde_json::json!({ "kind": "announced" })
    );
    assert!(
        snapshot["projection"]["closing_progress"]["remaining_ms"]
            .as_u64()
            .unwrap()
            <= sampled - 9_500
    );
    drop(fresh);
    drop(browser);
    drop(operator);
    running.stop(duded::ExitCause::ShutdownRpc);
}

#[test]
fn only_generation_three_upgrades_before_any_domain_frame() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let daemon = duded::boot::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().unwrap();
    let running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let before = fixture.entries().len();
    for generation in [1, 2, 4] {
        let protocols = format!("dudes-floor-v{generation}, dudes-csrf.{csrf}");
        let (mut stream, head) = raw_upgrade(addr, Some(&cookie), Some(&protocols), None);
        assert!(head.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{head:?}");
        assert!(!head.contains("Sec-WebSocket-Protocol:"), "{head:?}");
        let mut remainder = Vec::new();
        match stream.read_to_end(&mut remainder) {
            Ok(_) => {}
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
        }
        assert!(
            remainder.is_empty(),
            "a refused generation received bytes after its response"
        );
    }
    let protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");
    let (stream, head) = raw_upgrade(addr, Some(&cookie), Some(&protocols), None);
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "{head:?}"
    );
    assert!(head.contains("Sec-WebSocket-Protocol: dudes-floor-v3\r\n"));
    let mut browser = WebSocket { stream };
    browser.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(browser.json()["type"], "floor.snapshot");
    assert_eq!(fixture.entries().len(), before);
    drop(browser);
    drop(running);
}

#[test]
fn stream_admission_limits_refuse_then_recover() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);

    let mut open_sockets = Vec::new();
    for _ in 0..32 {
        let mut stream = TcpStream::connect(addr).expect("held TCP connection");
        stream.write_all(b"GET /floor/ HTTP/1.1\r\nHost:").unwrap();
        open_sockets.push(stream);
        if open_sockets.len() == 1 {
            wait_for_live(&probe, 1);
        }
    }
    wait_for_live(&probe, 32);
    let mut refused = TcpStream::connect(addr).expect("connection 33 reaches accept");
    refused
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    refused
        .write_all(format!("GET /floor/ HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .unwrap();
    let mut byte = [0_u8; 1];
    match refused.read(&mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ) => {}
        other => panic!("connection 33 was not actively refused before parse: {other:?}"),
    }
    drop(open_sockets.pop());
    wait_for_live(&probe, 31);
    let mut refill = TcpStream::connect(addr).expect("refill connection");
    refill
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    refill
        .write_all(format!("GET /floor/ HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .unwrap();
    assert!(
        read_head(&mut refill)
            .expect("refill response")
            .starts_with("HTTP/1.1 200 OK")
    );
    drop(open_sockets);
    wait_for_live(&probe, 0);

    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let first = WebSocket::connect(addr, &cookie, &csrf);
    let second = WebSocket::connect(addr, &cookie, &csrf);
    let protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");
    let (_third, response_head) = raw_upgrade(addr, Some(&cookie), Some(&protocols), None);
    assert!(
        response_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{response_head:?}"
    );
    drop(first);
    wait_for_live(&probe, 1);
    let refill = WebSocket::connect(addr, &cookie, &csrf);
    drop(refill);
    drop(second);
    wait_for_live(&probe, 0);

    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "both independent queue limits need exact-bound, overflow and refill controls"
)]
fn stream_queue_message_and_byte_limits_are_independent() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    // The subject here is the queue's own arithmetic — "exactly 256 deltas, still live" — and
    // since a heartbeat consumes that same bound. Turning the beat off keeps this
    // test counting only its own deltas rather than widening what it asserts to absorb an arrival
    // it cannot time. That heartbeats do share the bound is asserted by
    // `floor_stream::tests::a_never_draining_subscriber_is_closed_by_heartbeats_alone`.
    let fault = FloorStreamFault::default().without_heartbeat();
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
        None,
        fault.clone(),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let base = common::hire(&running.handle, fixture.state.path());
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);

    fault.hold_next_subscription();
    let mut message_bound = WebSocket::connect(addr, &cookie, &csrf);
    message_bound.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));
    for _ in 0..256 {
        pause(&running.handle, base);
    }
    assert_eq!(fault.queue_usage().map(|usage| usage.0), Some(256));
    assert_eq!(fault.queue_usage().map(|usage| usage.2), Some(false));
    pause(&running.handle, base);
    assert_eq!(fault.queue_usage().map(|usage| usage.2), Some(true));
    fault.release();
    assert_eq!(message_bound.json()["type"], "floor.snapshot");
    message_bound.expect_closed(&probe);
    drop(message_bound);
    wait_for_live(&probe, 0);

    fault.hold_next_subscription();
    let mut decrement = WebSocket::connect(addr, &cookie, &csrf);
    decrement.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));
    pause(&running.handle, base);
    assert!(
        fault
            .queue_usage()
            .is_some_and(|usage| usage.0 == 1 && usage.1 > 0)
    );
    fault.release();
    assert_eq!(decrement.json()["type"], "floor.snapshot");
    assert_eq!(decrement.json()["type"], "floor.delta");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while fault
        .queue_usage()
        .is_some_and(|usage| usage.0 != 0 || usage.1 != 0)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "dequeue did not release bytes"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    decrement.send(r#"{"type":"floor.unsubscribe"}"#);
    decrement.expect_closed(&probe);
    drop(decrement);
    wait_for_live(&probe, 0);

    fault.hold_next_subscription();
    let mut calibration = WebSocket::connect(addr, &cookie, &csrf);
    calibration.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));
    let sample = hire_with_role(&running.handle, fixture.state.path(), "x".to_owned());
    let sample_frame = fault.queue_usage().expect("calibration queue").1;
    assert!(sample_frame > 5 && sample_frame < 65_536);
    let json_overhead = sample_frame - 1 - 4;
    retire(&running.handle, sample);
    fault.release();
    drop(calibration);
    wait_for_live(&probe, 0);

    fault.hold_next_subscription();
    let mut byte_bound = WebSocket::connect(addr, &cookie, &csrf);
    byte_bound.send(r#"{"type":"floor.subscribe"}"#);
    assert!(fault.wait_registered(Duration::from_secs(2)));

    loop {
        let used = fault.queue_usage().expect("byte-bound queue").1;
        let remaining = MAX_QUEUE_BYTES - used;
        if remaining <= 1_000_000 {
            break;
        }
        let target_frame = 900_000usize.min(remaining - 100_000);
        let role_len = target_frame
            .checked_sub(json_overhead + 10)
            .expect("large frame has payload room");
        let agent = hire_with_role(&running.handle, fixture.state.path(), "x".repeat(role_len));
        retire(&running.handle, agent);
        assert_eq!(fault.queue_usage().map(|usage| usage.2), Some(false));
    }

    let before_calibration = fault.queue_usage().expect("queue before calibration").1;
    let final_sample = hire_with_role(&running.handle, fixture.state.path(), "x".to_owned());
    let after_calibration = fault.queue_usage().expect("queue after calibration").1;
    let current_overhead = after_calibration - before_calibration - 1 - 4;
    retire(&running.handle, final_sample);
    let used = fault.queue_usage().expect("queue before exact fill").1;
    let remaining = MAX_QUEUE_BYTES - used;
    let header = if remaining - current_overhead <= 65_535 {
        4
    } else {
        10
    };
    let final_role_len = remaining
        .checked_sub(current_overhead + header)
        .expect("exact final frame has payload room");
    let _last = hire_with_role(
        &running.handle,
        fixture.state.path(),
        "x".repeat(final_role_len),
    );
    assert_eq!(
        fault.queue_usage(),
        Some((
            fault.queue_usage().expect("queue").0,
            MAX_QUEUE_BYTES,
            false
        )),
        "the exact complete-frame byte bound stays live"
    );
    assert!(
        fault.queue_usage().expect("queue").0 < 256,
        "the byte arm fires independently below the message cap"
    );
    pause(&running.handle, base);
    assert_eq!(fault.queue_usage().map(|usage| usage.2), Some(true));
    fault.release();
    assert_eq!(byte_bound.json()["type"], "floor.snapshot");
    byte_bound.expect_closed(&probe);
    drop(byte_bound);
    wait_for_live(&probe, 0);

    let mut refill = WebSocket::connect(addr, &cookie, &csrf);
    refill.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(refill.json()["type"], "floor.snapshot");
    refill.send(r#"{"type":"floor.unsubscribe"}"#);
    refill.expect_closed(&probe);
    drop(refill);
    wait_for_live(&probe, 0);
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn stream_idle_and_absolute_expiry_release_slots() {
    const MINUTE_MS: i64 = 60_000;

    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());

    let quiet_issue = issue(&settings);
    let (quiet_cookie, quiet_csrf) = exchange(addr, &quiet_issue.bootstrap);
    let mut quiet = WebSocket::connect(addr, &quiet_cookie, &quiet_csrf);
    wait_for_live(&probe, 1);
    clock.advance(30 * MINUTE_MS);
    quiet.expect_closed(&probe);
    drop(quiet);
    wait_for_live(&probe, 0);

    let outbound_issue = issue(&settings);
    let (outbound_cookie, outbound_csrf) = exchange(addr, &outbound_issue.bootstrap);
    let mut outbound = WebSocket::connect(addr, &outbound_cookie, &outbound_csrf);
    outbound.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(outbound.json()["type"], "floor.snapshot");
    clock.advance(29 * MINUTE_MS);
    pause(&running.handle, agent);
    assert_eq!(outbound.json()["type"], "floor.delta");
    clock.advance(MINUTE_MS);
    outbound.expect_closed(&probe);
    drop(outbound);
    wait_for_live(&probe, 0);

    let absolute_issue = issue(&settings);
    let (absolute_cookie, absolute_csrf) = exchange(addr, &absolute_issue.bootstrap);
    let mut held_socket = WebSocket::connect(addr, &absolute_cookie, &absolute_csrf);
    for _ in 0..16 {
        clock.advance(29 * MINUTE_MS);
        let activity = WebSocket::connect(addr, &absolute_cookie, &absolute_csrf);
        drop(activity);
        wait_for_live(&probe, 1);
    }
    clock.advance(16 * MINUTE_MS);
    held_socket.expect_closed(&probe);
    drop(held_socket);
    wait_for_live(&probe, 0);
    let protocols = format!("dudes-floor-v3, dudes-csrf.{absolute_csrf}");
    let (_expired, response_head) =
        raw_upgrade(addr, Some(&absolute_cookie), Some(&protocols), None);
    assert!(
        response_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{response_head:?}"
    );

    let refill_issue = issue(&settings);
    let (refill_cookie, refill_csrf) = exchange(addr, &refill_issue.bootstrap);
    let first = WebSocket::connect(addr, &refill_cookie, &refill_csrf);
    let second = WebSocket::connect(addr, &refill_cookie, &refill_csrf);
    drop(first);
    drop(second);
    wait_for_live(&probe, 0);
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn stalled_upgraded_streams_stop_within_join_bound() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);

    let idle = WebSocket::connect(addr, &cookie, &csrf);
    let mut non_reading = WebSocket::connect(addr, &cookie, &csrf);
    non_reading.send(r#"{"type":"floor.subscribe"}"#);
    wait_for_live(&probe, 2);
    for _ in 0..4 {
        let agent = hire_with_role(&running.handle, fixture.state.path(), "x".repeat(700_000));
        retire(&running.handle, agent);
    }

    let started = std::time::Instant::now();
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
    assert!(
        started.elapsed() <= duded::floor_gateway::JOIN_BOUND,
        "upgraded peers exceeded the Floor join bound: {:?}",
        started.elapsed()
    );
    assert_eq!(probe.live_connections(), 0);
    drop(idle);
    drop(non_reading);
}

#[test]
fn close_oracle_refuses_open_peer() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut open = WebSocket::connect(addr, &cookie, &csrf);
    wait_for_live(&probe, 1);
    open.stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        open.expect_closed(&probe);
    }));
    assert!(
        observed.is_err(),
        "a read timeout must not satisfy the close oracle"
    );
    assert_eq!(probe.live_connections(), 1, "the peer is deliberately open");
    drop(open);
    wait_for_live(&probe, 0);
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn connection_spawn_failure_stops_existing_streams() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, port);
    let fault = duded::floor_gateway::FloorStartFault::connection_spawn_failure();
    let daemon = duded::boot::boot_with_floor_fault(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
        Some(fault.clone()),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("gateway probe");
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut held = WebSocket::connect(addr, &cookie, &csrf);
    held.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(held.json()["type"], "floor.snapshot");
    wait_for_live(&probe, 1);

    let started = std::time::Instant::now();
    fault.fail_next_connection_spawn();
    let _refused = TcpStream::connect(addr).expect("the armed connection reaches accept");
    held.expect_closed(&probe);
    assert!(
        started.elapsed() <= duded::floor_gateway::JOIN_BOUND,
        "the upgraded peer outlived the fatal spawn failure: {:?}",
        started.elapsed()
    );
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::FloorGatewayEnded,
        "the fatal spawn failure reached the daemon before any later stop"
    );
    drop(held);
}

#[test]
fn append_poison_publishes_only_the_stopped_fact_and_keeps_authenticated_reads() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, free_port());
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = duded::boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.append_failure();
    let probe = daemon.floor_gateway_probe().unwrap();
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let addr: SocketAddr = issued
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    browser.send(r#"{"type":"floor.subscribe"}"#);
    let before = browser.json();
    assert_eq!(before["type"], "floor.snapshot");
    assert!(before.get("journal_stopped").is_none());
    failure.short_write_next(5);
    assert!(
        running
            .handle
            .ask(Ask::Hire(Box::new(hire_params(fixture.state.path()))))
            .is_err()
    );
    let attempted = running
        .handle
        .doctor()
        .unwrap()
        .journal_stopped
        .unwrap()
        .lsn;
    let publication = loop {
        let value = browser.json();
        if value["type"] != "floor.heartbeat" {
            break value;
        }
    };
    assert_eq!(publication["type"], "floor.delta");
    assert_eq!(publication["delta"]["lsn"], before["projection"]["lsn"]);
    assert_eq!(publication["messages_lsn"], before["messages_lsn"]);
    assert_eq!(
        publication["journal_stopped"],
        serde_json::json!({"lsn": attempted.0.to_string()})
    );
    let keys: Vec<_> = publication
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "delta",
            "journal_stopped",
            "messages_lsn",
            "threads_lsn",
            "type"
        ]
    );
    let fact = serde_json::json!({"journal_stopped": publication["journal_stopped"]});
    assert!(serde_json::to_vec(&fact).unwrap().len() < 64);
    let mut second = WebSocket::connect(addr, &cookie, &csrf);
    second.send(r#"{"type":"floor.subscribe"}"#);
    let snapshot = second.json();
    assert_eq!(snapshot["type"], "floor.snapshot");
    assert_eq!(snapshot["projection"], before["projection"]);
    assert_eq!(snapshot["journal_stopped"], publication["journal_stopped"]);
    let (_, unauthenticated) = raw_upgrade(addr, None, Some("dudes-floor-v3"), None);
    assert!(!unauthenticated.starts_with("HTTP/1.1 101"));
    assert!(!unauthenticated.contains("journal_stopped"));
    let refused = running
        .handle
        .ask(Ask::Hire(Box::new(hire_params(fixture.state.path()))))
        .unwrap_err();
    assert_eq!(refused.code, dudes_proto::rpc::JOURNAL_STOPPED_CODE);
    assert_eq!(
        refused.data.unwrap(),
        serde_json::json!({"lsn": attempted.0})
    );
    drop(second);
    wait_for_live(&probe, 1);
    clock.advance(30 * 60_000);
    browser.expect_closed(&probe);
    drop(browser);
    running.stop(duded::ExitCause::Ended);
}

#[test]
fn checkpoint_poison_keeps_the_stopped_fact_after_durable_projection_publication() {
    let fixture = Fixture::new();
    let mut settings = settings(&fixture, free_port());
    settings.journal.checkpoint_every = 1;
    let host = SharedHost::new();
    let daemon = duded::boot(
        &settings,
        seams(
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.append_failure();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &issued.bootstrap);
    let mut browser = WebSocket::connect(addr, &cookie, &csrf);
    browser.send(r#"{"type":"floor.subscribe"}"#);
    let before = browser.json();
    failure.short_write_after(1, 5);
    let _ = running
        .handle
        .ask(Ask::Hire(Box::new(hire_params(fixture.state.path()))));
    let attempted = running
        .handle
        .doctor()
        .unwrap()
        .journal_stopped
        .unwrap()
        .lsn;
    let notification = loop {
        let next = browser.json();
        if next["type"] != "floor.heartbeat" {
            break next;
        }
    };
    assert_eq!(
        notification["journal_stopped"],
        serde_json::json!({"lsn":attempted.0.to_string()})
    );
    assert_eq!(notification["delta"]["lsn"], before["projection"]["lsn"]);
    let mut second = WebSocket::connect(addr, &cookie, &csrf);
    second.send(r#"{"type":"floor.subscribe"}"#);
    let snapshot = second.json();
    assert_eq!(snapshot["journal_stopped"], notification["journal_stopped"]);
    assert!(snapshot["projection"]["lsn"].as_u64().unwrap() < attempted.0);
    assert!(host.spawned().is_empty());
    drop(second);
    drop(browser);
    running.stop(duded::ExitCause::Ended);
}
