//! The operator bootstrap through the shipped RPC and HTTP interfaces.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use duded::floor_gateway::FloorGatewayProbe;
use duded::settings::FloorSettings;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::{SESSION_BOOTSTRAP_LEN, SessionIssueResult};
use dudes_proto::rpc::{CurrentMethod, ErrorCode, Method, RpcError};
use dudes_rpc::Client;
use dudes_supervisor::testing::FakeRng;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock, goal_params, seams};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);
const MINUTE_MS: i64 = 60_000;

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    listener.local_addr().expect("reserved address").port()
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

fn raw_exchange(addr: SocketAddr, host: &str, origin: Option<&str>, bootstrap: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n",
        bootstrap.len()
    );
    if let Some(origin) = origin {
        use std::fmt::Write as _;
        let _ = write!(&mut request, "Origin: {origin}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write head");
    stream
        .write_all(bootstrap.as_bytes())
        .expect("write bootstrap");
    stream.shutdown(Shutdown::Write).expect("finish request");
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset, "{error}");
    }
    response
}

fn raw_recover(
    addr: SocketAddr,
    host: &str,
    origin: Option<&str>,
    cookie: Option<&str>,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut request =
        format!("POST /floor/session/recover HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\n");
    if let Some(origin) = origin {
        use std::fmt::Write as _;
        let _ = write!(&mut request, "Origin: {origin}\r\n");
    }
    if let Some(cookie) = cookie {
        use std::fmt::Write as _;
        let _ = write!(&mut request, "Cookie: dudes_floor_session={cookie}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write recovery");
    stream.shutdown(Shutdown::Write).expect("finish recovery");
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset, "{error}");
    }
    response
}

fn response_parts(response: &[u8]) -> (&str, &[u8]) {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("complete response head")
        + 4;
    (
        std::str::from_utf8(&response[..end]).expect("ASCII response head"),
        &response[end..],
    )
}

fn assert_empty_refusal(response: &[u8]) {
    let (head, body) = response_parts(response);
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "unexpected refusal: {head:?}"
    );
    assert_eq!(body, b"");
    assert!(!head.contains("Set-Cookie:"));
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));
}

fn exchange(addr: SocketAddr, host: &str, origin: &str, bootstrap: &str) -> (String, String) {
    let response = raw_exchange(addr, host, Some(origin), bootstrap);
    let (head, body) = response_parts(&response);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
    let set_cookie = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("session cookie");
    assert!(set_cookie.contains("; Path=/floor/"), "{set_cookie}");
    assert!(set_cookie.contains("; HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("; SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("; Max-Age=28800"), "{set_cookie}");
    assert!(
        !set_cookie.contains("Domain="),
        "host-only cookie: {set_cookie}"
    );
    assert!(
        !set_cookie.contains("Secure"),
        "plain loopback HTTP: {set_cookie}"
    );
    let cookie = set_cookie
        .strip_prefix("dudes_floor_session=")
        .expect("cookie name")
        .split(';')
        .next()
        .expect("cookie value")
        .to_owned();
    let json: serde_json::Value = serde_json::from_slice(body).expect("JSON exchange result");
    assert_eq!(json.as_object().map(serde_json::Map::len), Some(1));
    let csrf = json["csrf"].as_str().expect("CSRF nonce").to_owned();
    assert_eq!(cookie.len(), SESSION_BOOTSTRAP_LEN);
    assert_eq!(csrf.len(), SESSION_BOOTSTRAP_LEN);
    (cookie, csrf)
}

fn settings(fixture: &Fixture, port: Option<u16>) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port,
    });
    settings
}

/// This scenario crosses the real operator-classified RPC route and loopback parser.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the acceptance command names this one ordered end-to-end scenario"
)]
fn bootstrap_exchange_is_single_use_origin_bound_and_expiring() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("first daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let sessions = daemon
        .floor_session_authorizer()
        .expect("Floor session authorizer");
    let mut running = common::start(daemon);

    let mut unproven = Client::connect(settings.socket()).expect("unproven client connects");
    let refused = unproven
        .call(Method::FloorSessionIssue, None)
        .expect("unproven issue call is answered");
    assert!(refused.result.is_none() && refused.error.is_some());

    let first = issue(&settings);
    assert_eq!(first.base_url, origin);
    assert_eq!(first.expires_in_s, 60);
    assert_eq!(first.bootstrap.len(), SESSION_BOOTSTRAP_LEN);
    assert_empty_refusal(&raw_exchange(
        addr,
        "localhost.invalid",
        Some(&origin),
        &first.bootstrap,
    ));
    assert_empty_refusal(&raw_exchange(
        addr,
        &host,
        Some("http://localhost.invalid"),
        &first.bootstrap,
    ));
    assert_empty_refusal(&raw_exchange(addr, &host, None, &first.bootstrap));
    assert_empty_refusal(&raw_exchange(addr, &host, Some("null"), &first.bootstrap));
    let (cookie, csrf) = exchange(addr, &host, &origin, &first.bootstrap);
    assert!(
        sessions
            .authorize_read("localhost.invalid", &origin, &cookie, &csrf)
            .is_none()
    );
    assert!(
        sessions
            .authorize_read(&host, "http://localhost.invalid", &cookie, &csrf)
            .is_none()
    );
    assert!(
        sessions
            .authorize_read(&host, &origin, &cookie, &"A".repeat(43))
            .is_none()
    );
    assert!(
        sessions
            .authorize_read(&host, &origin, &cookie, &csrf)
            .is_some(),
        "the exact session grants the opaque read-only permit"
    );
    assert_empty_refusal(&raw_exchange(addr, &host, Some(&origin), &first.bootstrap));

    let stale_bootstrap = issue(&settings);
    clock.advance(61_000);
    assert_empty_refusal(&raw_exchange(
        addr,
        &host,
        Some(&origin),
        &stale_bootstrap.bootstrap,
    ));

    let idle = issue(&settings);
    let (idle_cookie, idle_csrf) = exchange(addr, &host, &origin, &idle.bootstrap);
    clock.advance(30 * MINUTE_MS);
    assert!(
        sessions
            .authorize_read(&host, &origin, &idle_cookie, &idle_csrf)
            .is_none(),
        "30 minutes idle expires the session"
    );

    let active = issue(&settings);
    let (active_cookie, active_csrf) = exchange(addr, &host, &origin, &active.bootstrap);
    for _ in 0..16 {
        clock.advance(29 * MINUTE_MS);
        assert!(
            sessions
                .authorize_read(&host, &origin, &active_cookie, &active_csrf)
                .is_some(),
            "activity within the idle window keeps the session live"
        );
    }
    clock.advance(16 * MINUTE_MS);
    assert!(
        sessions
            .authorize_read(&host, &origin, &active_cookie, &active_csrf)
            .is_none(),
        "eight hours absolute expires even an active session"
    );

    let before_restart = issue(&settings);
    let (old_cookie, old_csrf) = exchange(addr, &host, &origin, &before_restart.bootstrap);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    assert!(
        sessions
            .authorize_read(&host, &origin, &old_cookie, &old_csrf)
            .is_none(),
        "stopping the owning run invalidates every retained authorizer"
    );

    let second = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            clock,
            Witness::default(),
        ),
    )
    .expect("second daemon boot on the same origin");
    assert_eq!(second.floor_addr(), Some(addr));
    let restarted_sessions = second
        .floor_session_authorizer()
        .expect("restarted Floor session authorizer");
    assert!(
        restarted_sessions
            .authorize_read(&host, &origin, &old_cookie, &old_csrf)
            .is_none(),
        "a new daemon run does not accept the old session"
    );
    common::start(second).stop(ExitCause::ShutdownRpc);
}

/// How long a lost browser has to reach the gateway's own connection accounting.
const CLOSE_BOUND: Duration = Duration::from_secs(3);

/// The extension offer a native browser appends to its own opening handshake.
const NATIVE_EXTENSION_OFFER: &str = "permessage-deflate; client_max_window_bits";

/// Offer the `/floor/stream` upgrade with an exact carrier, and return the socket and response head.
fn raw_upgrade(
    addr: SocketAddr,
    host: &str,
    origin: &str,
    cookie: Option<&str>,
    protocols: &str,
) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Extensions: {NATIVE_EXTENSION_OFFER}\r\nSec-WebSocket-Protocol: {protocols}\r\n"
    );
    if let Some(cookie) = cookie {
        use std::fmt::Write as _;
        let _ = write!(&mut request, "Cookie: dudes_floor_session={cookie}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the upgrade request");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .expect("upgrade response head byte");
        head.push(byte[0]);
    }
    (
        stream,
        String::from_utf8(head).expect("ASCII response head"),
    )
}

/// One masked client text frame, the only shape a browser is allowed to send.
fn masked_text(payload: &[u8]) -> Vec<u8> {
    let mask = [1_u8, 2, 3, 4];
    let mut frame = vec![
        0x81,
        0x80 | u8::try_from(payload.len()).expect("a short browser message"),
    ];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    frame
}

/// The next server text frame's payload.
fn server_text(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).expect("server frame head");
    assert_eq!(head[0] & 0x0f, 1, "the read-only stream emits text");
    assert_eq!(head[1] & 0x80, 0, "server frames are not masked");
    let len = match head[1] & 0x7f {
        value @ 0..=125 => usize::from(value),
        126 => {
            let mut bytes = [0_u8; 2];
            stream.read_exact(&mut bytes).expect("u16 frame length");
            usize::from(u16::from_be_bytes(bytes))
        }
        _ => {
            let mut bytes = [0_u8; 8];
            stream.read_exact(&mut bytes).expect("u64 frame length");
            usize::try_from(u64::from_be_bytes(bytes)).expect("the frame fits usize")
        }
    };
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).expect("server frame body");
    payload
}

/// Block until the gateway itself owns `expected` connections, or fail.
///
/// This reads the gateway's own slot counter and never writes to the connection, so waiting here
/// cannot be what ends the stream under test.
fn wait_for_live(probe: &FloorGatewayProbe, expected: usize) {
    let deadline = std::time::Instant::now() + CLOSE_BOUND;
    while probe.live_connections() != expected {
        assert!(
            std::time::Instant::now() < deadline,
            "expected {expected} live Floor connections, the gateway owns {}",
            probe.live_connections()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn recover_cookie_only_reaches_snapshot() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let daemon = boot(
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
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    let issued = issue(&settings);
    let (cookie, old_csrf) = exchange(addr, &host, &origin, &issued.bootstrap);

    assert_empty_refusal(&raw_recover(addr, &host, None, Some(&cookie)));
    assert_empty_refusal(&raw_recover(
        addr,
        &host,
        Some("http://localhost.invalid"),
        Some(&cookie),
    ));

    let response = raw_recover(addr, &host, Some(&origin), Some(&cookie));
    let (head, body) = response_parts(&response);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
    assert!(head.contains("Cache-Control: no-store\r\n"), "{head:?}");
    assert!(head.contains("Content-Security-Policy: "), "{head:?}");
    assert!(
        head.contains("X-Content-Type-Options: nosniff\r\n"),
        "{head:?}"
    );
    assert!(
        head.contains("Referrer-Policy: no-referrer\r\n"),
        "{head:?}"
    );
    assert!(
        head.contains("Cross-Origin-Resource-Policy: same-origin\r\n"),
        "{head:?}"
    );
    assert!(!head.contains("Set-Cookie:"), "{head:?}");
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));
    let json: serde_json::Value = serde_json::from_slice(body).expect("recovery JSON");
    assert_eq!(json.as_object().map(serde_json::Map::len), Some(1));
    let csrf = json["csrf"].as_str().expect("rotated CSRF");
    assert_eq!(csrf.len(), SESSION_BOOTSTRAP_LEN);
    assert_eq!(body, format!(r#"{{"csrf":"{csrf}"}}"#).as_bytes());

    let old_protocols = format!("dudes-floor-v3, dudes-csrf.{old_csrf}");
    let (old, old_head) = raw_upgrade(addr, &host, &origin, Some(&cookie), &old_protocols);
    assert!(
        old_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{old_head:?}"
    );
    drop(old);

    let protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");
    let (mut browser, upgrade) = raw_upgrade(addr, &host, &origin, Some(&cookie), &protocols);
    assert!(
        upgrade.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "{upgrade:?}"
    );
    browser
        .write_all(&masked_text(br#"{"type":"floor.subscribe"}"#))
        .expect("the recovered browser subscribes");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&server_text(&mut browser)).expect("snapshot JSON");
    assert_eq!(snapshot["type"], "floor.snapshot", "{snapshot}");
    assert_eq!(
        snapshot["projection"]["agents"][0]["id"],
        serde_json::Value::String(agent.to_string()),
        "the recovered stream serves the live projection"
    );
    drop(browser);

    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Losing an unattached browser appends no domain record.
///
/// The connection this test loses is one the **daemon** authenticated. A `101` on `/floor/stream` is
/// granted only against the session cookie and the CSRF subprotocol, and the cookie-less arm below
/// establishes that by observation rather than by this comment: the same request without the cookie
/// is refused. Losing it is a `drop` — the way a browser goes — with no close frame, no unsubscribe
/// and no endpoint. Dropping the socket does not revoke the server session and nothing here claims
/// it does.
///
/// The oracle is whole-journal equality, so an appended record of **any** kind fails it. An absence
/// proves nothing about a journal nobody is reading, so the same comparison is then applied to a
/// window that does contain an append: `goal.set`'s own returned `(AgentId, GoalId)` — a value the
/// subject's journal did not supply — must appear in a non-empty suffix that preserves the prefix.
#[test]
fn unattached_browser_close_appends_no_domain_record() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let daemon = boot(
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
    let probe = daemon.floor_gateway_probe().expect("Floor gateway probe");
    let mut running = common::start(daemon);

    // No goal: a goal-less agent issues no turn, so nothing but the browser can reach the journal.
    let agent = common::hire(&running.handle, fixture.state.path());
    let issued = issue(&settings);
    let (cookie, csrf) = exchange(addr, &host, &origin, &issued.bootstrap);
    let protocols = format!("dudes-floor-v3, dudes-csrf.{csrf}");

    let (refused, refused_head) = raw_upgrade(addr, &host, &origin, None, &protocols);
    assert!(
        refused_head.starts_with("HTTP/1.1 403 Forbidden"),
        "an upgrade without the session cookie reached the stream: {refused_head:?}"
    );
    drop(refused);
    wait_for_live(&probe, 0);

    let (mut browser, head) = raw_upgrade(addr, &host, &origin, Some(&cookie), &protocols);
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "the authenticated carrier did not reach the stream: {head:?}"
    );
    assert!(
        head.contains("Sec-WebSocket-Protocol: dudes-floor-v3\r\n"),
        "{head:?}"
    );
    browser
        .write_all(&masked_text(br#"{"type":"floor.subscribe"}"#))
        .expect("the browser subscribes");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&server_text(&mut browser)).expect("server JSON");
    assert_eq!(snapshot["type"], "floor.snapshot", "{snapshot}");
    assert_eq!(
        snapshot["projection"]["agents"].as_array().map(Vec::len),
        Some(1),
        "the authenticated browser is serving this run's projection"
    );
    wait_for_live(&probe, 1);

    let before_close = fixture.entries();
    assert!(
        !before_close
            .iter()
            .any(|(_, entry)| matches!(entry.record, Record::Attached { .. })),
        "the scenario is an unattached browser: {:?}",
        before_close
            .iter()
            .map(|(_, entry)| entry.record.kind())
            .collect::<Vec<_>>()
    );

    drop(browser);
    wait_for_live(&probe, 0);

    let after_close = fixture.entries();
    assert_eq!(
        after_close, before_close,
        "losing an unattached browser appended a domain record"
    );

    let goal = common::set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    fixture.await_record("the control GoalSet", |record| match record {
        Record::GoalSet {
            agent: owner,
            goal: set,
            ..
        } if *owner == agent && set.id == goal => Some(()),
        _ => None,
    });
    let after_goal = fixture.entries();
    assert_eq!(
        after_goal.get(..after_close.len()),
        Some(after_close.as_slice()),
        "the control must extend the tail the close window observed, not replace it"
    );
    let appended = &after_goal[after_close.len()..];
    assert!(
        appended.iter().any(|(_, entry)| matches!(
            &entry.record,
            Record::GoalSet { agent: owner, goal: set, .. } if *owner == agent && set.id == goal
        )),
        "positive control: that tail is live; its suffix carries {:?}",
        appended
            .iter()
            .map(|(_, entry)| entry.record.kind())
            .collect::<Vec<_>>()
    );

    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

type TestDaemon = duded::Daemon<SharedHost, ScriptedRunner, TestClock, FakeRng>;

fn boot_floor(settings: &duded::Settings) -> anyhow::Result<TestDaemon> {
    boot(
        settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
}

/// One daemon run: the Floor port it bound, checked against the origin it issues.
fn floor_port_of_one_run(settings: &duded::Settings) -> u16 {
    let daemon = boot_floor(settings).expect("the daemon boots");
    let port = daemon.floor_addr().expect("Floor address").port();
    let mut running = common::start(daemon);
    assert_eq!(issue(settings).base_url, format!("http://127.0.0.1:{port}"));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    port
}

/// With no `DUDES_FLOOR_PORT`, a restarted daemon serves the Floor on the port it bound last, so
/// a browser keeps what it stored for that origin. The kept port is owner-only.
#[test]
fn a_restarted_daemon_serves_the_floor_on_the_port_it_bound_last() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, None);
    let kept = settings.state_dir.join("floor-port");
    std::fs::create_dir_all(&settings.state_dir).expect("the state directory");
    std::fs::write(&kept, "not a port\n").expect("an unreadable port record");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o644))
            .expect("a record others can read");
    }

    let first = floor_port_of_one_run(&settings);
    let second = floor_port_of_one_run(&settings);
    assert_eq!(second, first, "the restarted Floor keeps its origin");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&kept)
            .expect("the kept port")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the kept port is owner-only");
    }
}

/// A remembered port another process holds does not stop startup: the Floor takes a free port,
/// and the next run asks for that one.
#[test]
fn a_taken_remembered_port_falls_back_to_a_free_one_and_keeps_it() {
    let fixture = Fixture::new();
    let settings = settings(&fixture, None);
    let first = floor_port_of_one_run(&settings);

    let squatter = TcpListener::bind(("127.0.0.1", first)).expect("take the remembered port");
    let second = floor_port_of_one_run(&settings);
    assert_ne!(second, first, "the daemon starts on another port");
    drop(squatter);

    assert_eq!(
        floor_port_of_one_run(&settings),
        second,
        "the fallback port is the one kept"
    );
}

/// `DUDES_FLOOR_PORT` wins over the remembered port, is kept like any bound port, and still stops
/// startup when it is taken.
#[test]
fn an_explicit_port_wins_over_the_remembered_one() {
    let fixture = Fixture::new();
    let unset = settings(&fixture, None);
    let first = floor_port_of_one_run(&unset);
    let explicit = {
        let _held = TcpListener::bind(("127.0.0.1", first)).expect("hold the remembered port");
        free_port()
    };
    let mut settings = unset.clone();
    settings.floor.as_mut().expect("a Floor").port = Some(explicit);

    assert_eq!(floor_port_of_one_run(&settings), explicit);
    assert_eq!(
        floor_port_of_one_run(&unset),
        explicit,
        "a later run with no port set asks for the port the explicit run bound"
    );

    let _squatter = TcpListener::bind(("127.0.0.1", explicit)).expect("take the explicit port");
    let Err(error) = boot_floor(&settings) else {
        panic!("a taken explicit port must stop startup");
    };
    assert!(
        format!("{error:#}").contains("binding the Floor listener"),
        "{error:#}"
    );
}

fn carry_call(
    settings: &duded::Settings,
    method: CurrentMethod,
    params: Option<serde_json::Value>,
    credential: Option<OperatorCredential>,
) -> Result<serde_json::Value, RpcError> {
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let response = client
        .call_current_presenting(method, params, credential)
        .expect("the daemon answers");
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(error),
        other => panic!("one answer or one error, not {other:?}"),
    }
}

fn carry_daemon(
    settings: &duded::Settings,
    clock: &Arc<TestClock>,
    column: &Arc<ScriptedColumn>,
) -> TestDaemon {
    boot(
        settings,
        common::seams_with(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(clock),
            Witness::default(),
            Arc::clone(column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
        ),
    )
    .expect("daemon boot")
}

fn recovered_csrf(addr: SocketAddr, host: &str, origin: &str, cookie: &str) -> String {
    let response = raw_recover(addr, host, Some(origin), Some(cookie));
    let (head, body) = response_parts(&response);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
    let json: serde_json::Value = serde_json::from_slice(body).expect("recovery JSON");
    json["csrf"].as_str().expect("rotated CSRF").to_owned()
}

/// What `dudes upgrade` does: export from the old run, stop it, start the next, import. A plain
/// restart between two other runs restores nothing.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered scenario across three daemon runs"
)]
fn an_upgrade_carries_open_sign_ins_through_the_operator_door_and_a_plain_restart_does_not() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);

    let daemon = carry_daemon(&settings, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let first = issue(&settings);
    let (cookie, csrf) = exchange(addr, &host, &origin, &first.bootstrap);
    let second = issue(&settings);
    let (other_cookie, _) = exchange(addr, &host, &origin, &second.bootstrap);

    let operator = || Some(credential_of(&settings));
    let exported = carry_call(
        &settings,
        CurrentMethod::FloorSessionExport,
        None,
        operator(),
    )
    .expect("the operator exports");
    let text = exported.to_string();
    assert!(!text.contains(&cookie) && !text.contains(&csrf), "{text}");
    assert_eq!(
        exported
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["origin", "run", "sessions"]
    );
    assert_eq!(exported["origin"], origin);
    let rows = exported["sessions"].as_array().expect("sessions");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(
            row.as_object()
                .expect("a row")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "check_runs",
                "created",
                "digest",
                "last_used",
                "ticket_assigns",
                "ticket_creates"
            ]
        );
    }
    let import = serde_json::json!({ "exported": exported });

    let unauthorized = ErrorCode::Unauthorized.code();
    for method in [
        CurrentMethod::FloorSessionExport,
        CurrentMethod::FloorSessionImport,
    ] {
        let params = (method == CurrentMethod::FloorSessionImport).then(|| import.clone());
        let unproven = carry_call(&settings, method, params.clone(), None).expect_err("unproven");
        assert_eq!(unproven.code, unauthorized, "{method}");
        column.set(PeerProvenance::Agent(agent));
        let as_agent = carry_call(&settings, method, params, operator()).expect_err("an agent");
        assert_eq!(as_agent.code, unauthorized, "{method}");
        column.set(PeerProvenance::Unresolvable);
    }

    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = carry_daemon(&settings, &clock, &column);
    assert_eq!(daemon.floor_addr(), Some(addr));
    let authorizer = daemon.floor_session_authorizer().expect("authorizer");
    let mut running = common::start(daemon);
    assert_empty_refusal(&raw_recover(addr, &host, Some(&origin), Some(&cookie)));

    let imported = carry_call(
        &settings,
        CurrentMethod::FloorSessionImport,
        Some(import.clone()),
        operator(),
    )
    .expect("the operator imports");
    assert_eq!(imported, serde_json::json!({ "restored": 2 }));
    assert!(
        authorizer
            .authorize_read(&host, &origin, &cookie, &csrf)
            .is_none(),
        "the old CSRF secret is refused"
    );
    let rotated = recovered_csrf(addr, &host, &origin, &cookie);
    assert!(
        authorizer
            .authorize_read(&host, &origin, &cookie, &rotated)
            .is_some()
    );
    let protocols = format!("dudes-floor-v3, dudes-csrf.{rotated}");
    let (mut browser, upgrade) = raw_upgrade(addr, &host, &origin, Some(&cookie), &protocols);
    assert!(upgrade.starts_with("HTTP/1.1 101 "), "{upgrade:?}");
    browser
        .write_all(&masked_text(br#"{"type":"floor.subscribe"}"#))
        .expect("subscribe");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&server_text(&mut browser)).expect("snapshot JSON");
    assert_eq!(
        snapshot["projection"]["agents"][0]["id"],
        serde_json::Value::String(agent.to_string()),
        "the carried sign-in sees the agent hired in the earlier run"
    );
    drop(browser);
    let again = carry_call(
        &settings,
        CurrentMethod::FloorSessionImport,
        Some(import),
        operator(),
    )
    .expect("a second import is answered");
    assert_eq!(again, serde_json::json!({ "restored": 0 }));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = carry_daemon(&settings, &clock, &column);
    let mut running = common::start(daemon);
    for carried in [&cookie, &other_cookie] {
        assert_empty_refusal(&raw_recover(addr, &host, Some(&origin), Some(carried)));
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
