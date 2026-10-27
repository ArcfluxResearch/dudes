//! The operator bootstrap through the shipped RPC and HTTP interfaces.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use duded::floor_browser::{Browser, Page};
use duded::floor_gateway::FloorGatewayProbe;
use duded::settings::FloorSettings;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::{SESSION_BOOTSTRAP_LEN, SessionIssueResult};
use dudes_proto::rpc::{CurrentMethod, ErrorCode, Method, RpcError};
use dudes_rpc::Client;
use dudes_supervisor::Clock as _;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock, goal_params, seams};

/// The moment every daemon in this file starts at, and so the moment its pages sign for.
const START_MS: i64 = 1_800_000_000_000;

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v10"
);

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

/// The exchange a page with a key of its own sends for `link`, signed at the time `clock` reads.
fn raw_link(
    addr: SocketAddr,
    host: &str,
    origin: &str,
    clock: &Arc<TestClock>,
    link: &str,
) -> Vec<u8> {
    let browser = Browser::new();
    let body = browser.enrolment(link, "Chrome on macOS");
    let lines = browser.enrolment_lines(&body, origin, clock.now().unix_millis());
    raw_exchange(addr, host, Some(origin), &lines, &body)
}

fn raw_exchange(
    addr: SocketAddr,
    host: &str,
    origin: Option<&str>,
    lines: &str,
    body: &str,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        use std::fmt::Write as _;
        let _ = write!(&mut request, "Origin: {origin}\r\n");
    }
    request.push_str(lines);
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(body.as_bytes()).expect("write the body");
    stream.shutdown(Shutdown::Write).expect("finish request");
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

/// Sign a browser in with `bootstrap` and check the cookie the exchange sets. The page signs for
/// the time `clock` reads.
fn exchange(
    addr: SocketAddr,
    host: &str,
    origin: &str,
    clock: &Arc<TestClock>,
    bootstrap: &str,
) -> Page {
    exchange_named(addr, host, origin, clock, bootstrap, "Chrome on macOS")
}

fn exchange_named(
    addr: SocketAddr,
    host: &str,
    origin: &str,
    clock: &Arc<TestClock>,
    bootstrap: &str,
    name: &str,
) -> Page {
    let browser = Browser::new();
    let body = browser.enrolment(bootstrap, name);
    let lines = browser.enrolment_lines(&body, origin, clock.now().unix_millis());
    let response = raw_exchange(addr, host, Some(origin), &lines, &body);
    let (head, body) = response_parts(&response);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
    let set_cookie = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("session cookie");
    assert!(set_cookie.contains("; Path=/floor/"), "{set_cookie}");
    assert!(set_cookie.contains("; HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("; SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("; Max-Age=86400"), "{set_cookie}");
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
    assert_eq!(
        json["device"],
        browser.device().to_string(),
        "the answer names the device and carries no secret"
    );
    assert_eq!(cookie.len(), SESSION_BOOTSTRAP_LEN);
    let clock = Arc::clone(clock);
    Page::new(addr, cookie, browser).on_clock(move || clock.now().unix_millis())
}

/// The presentation of `page`'s next signed action, for the registry directly.
fn presentation<'a>(
    page: &'a Page,
    host: &'a str,
    origin: &'a str,
) -> duded::floor_session::Presentation<'a> {
    page.browser.presentation(
        host,
        origin,
        &page.cookie,
        "POST",
        "/floor/test",
        &[],
        page.now_ms(),
    )
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
fn bootstrap_exchange_is_single_use_origin_bound_and_expiring() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
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
    assert_empty_refusal(&raw_link(
        addr,
        &host,
        "http://localhost.invalid",
        &clock,
        &first.bootstrap,
    ));
    let enrolling = Browser::new();
    let body = enrolling.enrolment(&first.bootstrap, "Chrome on macOS");
    let foreign = Browser::new().enrolment_lines(&body, &origin, clock.now().unix_millis());
    for lines in ["", foreign.as_str()] {
        let refused = raw_exchange(addr, &host, Some(&origin), lines, &body);
        let (head, answer) = response_parts(&refused);
        assert!(
            head.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{head:?}"
        );
        assert!(!head.contains("Set-Cookie:"), "{head:?}");
        assert_eq!(answer, br#"{"signature":"refused"}"#);
    }
    let page = exchange(addr, &host, &origin, &clock, &first.bootstrap);
    assert!(
        sessions
            .authorize_read(&presentation(&page, &host, &origin))
            .is_ok(),
        "the exact session grants the opaque read-only permit"
    );
    assert_empty_refusal(&raw_link(addr, &host, &origin, &clock, &first.bootstrap));

    let stale_bootstrap = issue(&settings);
    clock.advance(61_000);
    assert_empty_refusal(&raw_link(
        addr,
        &host,
        &origin,
        &clock,
        &stale_bootstrap.bootstrap,
    ));

    drop(unproven);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    assert!(
        sessions
            .authorize_read(&presentation(&page, &host, &origin))
            .is_err()
    );
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
    let (opcode, payload) = server_frame(stream);
    assert_eq!(opcode, 1, "the read-only stream emits text");
    payload
}

fn server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).expect("server frame head");
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
    (head[0] & 0x0f, payload)
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
fn a_signed_handshake_reaches_the_snapshot_and_a_cookie_alone_does_not() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    let issued = issue(&settings);
    let page = exchange(addr, &host, &origin, &clock, &issued.bootstrap);
    let cookie = page.cookie.clone();

    for (refused, expected) in [
        ("dudes-floor-v6".to_owned(), "HTTP/1.1 401 Unauthorized"),
        (
            format!(
                "dudes-floor-v3, dudes-csrf.{}",
                "A".repeat(SESSION_BOOTSTRAP_LEN)
            ),
            "HTTP/1.1 403 Forbidden",
        ),
        (
            "dudes-floor-v3, dudes-csrf.".to_owned(),
            "HTTP/1.1 403 Forbidden",
        ),
    ] {
        let (socket, head) = raw_upgrade(addr, &host, &origin, Some(&cookie), &refused);
        assert!(
            head.starts_with(expected),
            "the cookie without a signature gets no stream: {head:?}"
        );
        drop(socket);
    }

    let protocols = format!(
        "dudes-floor-v6, {}",
        page.signed("GET", "/floor/stream", &[]).protocol_entries()
    );
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

/// A check or a sign-out of the session, as the page sends it: the cookie and the signature, or
/// either of them left out.
fn post_standing(
    addr: SocketAddr,
    path: &str,
    host: &str,
    origin: &str,
    credentials: (Option<&str>, Option<&duded::floor_browser::Signed>),
) -> (String, String) {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n"
    );
    let (cookie, signed) = credentials;
    {
        use std::fmt::Write as _;
        if let Some(cookie) = cookie {
            let _ = write!(&mut request, "Cookie: dudes_floor_session={cookie}\r\n");
        }
        if let Some(signed) = signed {
            request.push_str(&signed.header_lines());
        }
    }
    request.push_str("\r\n{}");
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let (head, body) = response_parts(&response);
    (head.to_owned(), String::from_utf8_lossy(body).into_owned())
}

#[test]
fn signing_out_closes_the_stream_and_only_an_unknown_session_is_told_so() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("Floor gateway probe");
    let mut running = common::start(daemon);
    let page = exchange(addr, &host, &origin, &clock, &issue(&settings).bootstrap);
    let check = |origin: &str, cookie: Option<&str>, signed: bool| {
        let signed = signed.then(|| page.signed("POST", "/floor/session/check", b"{}"));
        post_standing(
            addr,
            "/floor/session/check",
            &host,
            origin,
            (cookie, signed.as_ref()),
        )
    };

    let (head, body) = check(&origin, Some(&page.cookie), true);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(
        head.contains("; Max-Age=86400"),
        "the cookie is renewed: {head}"
    );
    assert_eq!(body, r#"{"session":"live"}"#);

    let nobody = "A".repeat(SESSION_BOOTSTRAP_LEN);
    for cookie in [Some(nobody.as_str()), None] {
        let (head, body) = check(&origin, cookie, true);
        assert!(head.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{head}");
        assert_eq!(body, r#"{"session":"unknown"}"#);
    }
    let (head, body) = check(&origin, Some(&page.cookie), false);
    assert!(head.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{head}");
    assert_eq!(
        body, r#"{"signature":"refused"}"#,
        "a live device that did not sign is told so, not that it is signed out"
    );
    let (head, body) = check("http://localhost.invalid", Some(&page.cookie), true);
    assert!(head.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{head}");
    assert_eq!(
        body, "",
        "a refusal that is not about the session says nothing of it"
    );

    let protocols = format!(
        "dudes-floor-v6, {}",
        page.signed("GET", "/floor/stream", &[]).protocol_entries()
    );
    let (mut stream, upgrade) = raw_upgrade(addr, &host, &origin, Some(&page.cookie), &protocols);
    assert!(upgrade.starts_with("HTTP/1.1 101"), "{upgrade:?}");
    assert!(
        upgrade.contains("Sec-WebSocket-Protocol: dudes-floor-v6\r\n"),
        "{upgrade:?}"
    );
    wait_for_live(&probe, 1);

    let end = page.signed("POST", "/floor/session/end", b"{}");
    let (head, body) = post_standing(
        addr,
        "/floor/session/end",
        &host,
        &origin,
        (Some(&page.cookie), Some(&end)),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(
        head.contains("; Max-Age=0"),
        "the browser forgets the cookie: {head}"
    );
    assert_eq!(body, r#"{"session":"ended"}"#);
    wait_for_live(&probe, 0);
    stream
        .set_read_timeout(Some(CLOSE_BOUND))
        .expect("read timeout");
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);

    let (head, body) = check(&origin, Some(&page.cookie), true);
    assert!(head.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{head}");
    assert_eq!(body, r#"{"session":"unknown"}"#);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Losing an unattached browser appends no domain record.
///
/// The connection this test loses is one the **daemon** authenticated. A `101` on `/floor/stream` is
/// granted only against the session cookie and a signature, and the cookie-less arm below
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
#[expect(
    clippy::too_many_lines,
    reason = "one ordered scenario across a close and a control"
)]
fn unattached_browser_close_appends_no_domain_record() {
    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
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
    let page = exchange(addr, &host, &origin, &clock, &issued.bootstrap);
    let cookie = page.cookie.clone();
    let protocols = || {
        format!(
            "dudes-floor-v6, {}",
            page.signed("GET", "/floor/stream", &[]).protocol_entries()
        )
    };

    let (refused, refused_head) = raw_upgrade(addr, &host, &origin, None, &protocols());
    assert!(
        refused_head.starts_with("HTTP/1.1 403 Forbidden"),
        "an upgrade without the session cookie reached the stream: {refused_head:?}"
    );
    drop(refused);
    wait_for_live(&probe, 0);

    let (mut browser, head) = raw_upgrade(addr, &host, &origin, Some(&cookie), &protocols());
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "the authenticated carrier did not reach the stream: {head:?}"
    );
    assert!(
        head.contains("Sec-WebSocket-Protocol: dudes-floor-v6\r\n"),
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

type TestDaemon = duded::Daemon<SharedHost, ScriptedRunner, TestClock, common::FixtureRng>;

fn boot_floor(settings: &duded::Settings) -> anyhow::Result<TestDaemon> {
    boot(
        settings,
        seams(
            settings,
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
            settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(clock),
            Witness::default(),
            Arc::clone(column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
        ),
    )
    .expect("daemon boot")
}

/// What `dudes upgrade` does: export from the old run, stop it, start the next, import. The
/// devices come back from the journal on their own; the export carries only the counts.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered scenario across two daemon runs"
)]
fn an_upgrade_carries_counts_through_the_operator_door_and_the_devices_come_from_the_journal() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let port = free_port();
    let settings = settings(&fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);

    let daemon = carry_daemon(&settings, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let first = issue(&settings);
    let page = exchange(addr, &host, &origin, &clock, &first.bootstrap);
    let second = issue(&settings);
    let other = exchange(addr, &host, &origin, &clock, &second.bootstrap);

    let operator = || Some(credential_of(&settings));
    let exported = carry_call(
        &settings,
        CurrentMethod::FloorSessionExport,
        None,
        operator(),
    )
    .expect("the operator exports");
    let text = exported.to_string();
    assert!(
        !text.contains(&page.cookie) && !text.contains(page.browser.key()),
        "{text}"
    );
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
            ],
            "no secret digest leaves"
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
    for carried in [&page, &other] {
        assert!(
            authorizer
                .authorize_read(&presentation(carried, &host, &origin))
                .is_ok(),
            "a restart keeps a device signed in, with no import"
        );
    }
    let stranger_clock = Arc::clone(&clock);
    let stranger = Page::new(addr, page.cookie.clone(), Browser::new())
        .on_clock(move || stranger_clock.now().unix_millis());
    assert!(
        authorizer
            .authorize_read(&presentation(&stranger, &host, &origin))
            .is_err(),
        "and no other key works"
    );

    let imported = carry_call(
        &settings,
        CurrentMethod::FloorSessionImport,
        Some(import.clone()),
        operator(),
    )
    .expect("the operator imports");
    assert_eq!(imported, serde_json::json!({ "restored": 2 }));
    let again = carry_call(
        &settings,
        CurrentMethod::FloorSessionImport,
        Some(import),
        operator(),
    )
    .expect("a second import is answered");
    assert_eq!(again, serde_json::json!({ "restored": 0 }));

    let previous_release = serde_json::json!({ "exported": {
        "run": "an-earlier-run",
        "origin": origin,
        "sessions": [{
            "digest": "ab".repeat(32),
            "secret": "cd".repeat(32),
            "created": 1_799_999_000_000_i64,
            "last_used": 1_799_999_500_000_i64,
            "ticket_creates": 1,
            "ticket_assigns": 0,
            "check_runs": 0
        }]
    }});
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = carry_daemon(&settings, &clock, &column);
    let mut running = common::start(daemon);
    assert_eq!(
        carry_call(
            &settings,
            CurrentMethod::FloorSessionImport,
            Some(previous_release),
            operator(),
        )
        .expect("an export from the previous release imports"),
        serde_json::json!({ "restored": 0 }),
        "its sessions are ignored"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn rpc(
    settings: &duded::Settings,
    method: Method,
    params: Option<serde_json::Value>,
    credential: Option<OperatorCredential>,
) -> Result<serde_json::Value, RpcError> {
    carry_call(settings, CurrentMethod::from(method), params, credential)
}

fn device_boot(
    fixture: &Fixture,
    clock: &Arc<TestClock>,
    column: &Arc<ScriptedColumn>,
) -> (duded::Settings, TestDaemon, String, String) {
    let port = free_port();
    let settings = settings(fixture, Some(port));
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let daemon = carry_daemon(&settings, clock, column);
    (settings, daemon, host, origin)
}

/// A device lives in the journal: a restart keeps it signed in, with its cookie and key
/// unchanged, and one that was removed stays removed.
#[test]
fn a_device_survives_a_restart_and_a_removed_one_stays_removed() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let (settings, daemon, host, origin) = device_boot(&fixture, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let authorizer = daemon.floor_session_authorizer().expect("authorizer");
    let mut running = common::start(daemon);
    let named = |name: &str| {
        exchange_named(
            addr,
            &host,
            &origin,
            &clock,
            &issue(&settings).bootstrap,
            name,
        )
    };
    let kept = named("Chrome on macOS");
    let gone = named("Firefox on Linux");
    assert!(
        authorizer
            .authorize_read(&presentation(&kept, &host, &origin))
            .is_ok()
    );

    let listed = rpc(
        &settings,
        Method::FloorDevicesList,
        None,
        Some(credential_of(&settings)),
    )
    .expect("the operator lists");
    let devices = listed["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 2);
    let id_of = |name: &str| {
        devices
            .iter()
            .find(|device| device["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed"))["device"]
            .as_str()
            .expect("an id")
            .to_owned()
    };
    assert_eq!(
        id_of("Chrome on macOS"),
        kept.browser.device().to_string(),
        "the list shows the id the browser computed from its own key"
    );
    assert!(
        devices
            .iter()
            .find(|device| device["name"] == "Chrome on macOS")
            .expect("listed")
            .get("first_used")
            .is_some(),
        "the used one shows its first use"
    );
    let removed = rpc(
        &settings,
        Method::FloorDevicesRemove,
        Some(serde_json::json!({ "device": id_of("Firefox on Linux") })),
        Some(credential_of(&settings)),
    )
    .expect("the operator removes");
    assert_eq!(removed, serde_json::json!({ "removed": true }));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = carry_daemon(&settings, &clock, &column);
    let authorizer = daemon.floor_session_authorizer().expect("authorizer");
    let mut running = common::start(daemon);
    assert!(
        authorizer
            .authorize_read(&presentation(&kept, &host, &origin))
            .is_ok(),
        "the device is still signed in after a restart"
    );
    assert!(
        authorizer
            .authorize_read(&presentation(&gone, &host, &origin))
            .is_err(),
        "a removed device stays removed"
    );
    let kinds: Vec<_> = fixture
        .device_records()
        .iter()
        .map(Record::kind)
        .map(|kind| format!("{kind:?}"))
        .collect();
    assert_eq!(
        kinds,
        [
            "FloorDeviceKeyEnrolled",
            "FloorDeviceKeyEnrolled",
            "FloorDeviceFirstUsed",
            "FloorDeviceRemoved"
        ],
        "two enrolled, one first used, one removed, and nothing else"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Listing, removing and keeping browsers are operator calls: an unproven peer and an agent are refused.
#[test]
fn the_device_rpcs_refuse_an_unproven_peer_and_an_agent() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let (settings, daemon, _, _) = device_boot(&fixture, &clock, &column);
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let unauthorized = ErrorCode::Unauthorized.code();
    let named = serde_json::json!({ "device": "A".repeat(43) });
    for (method, params) in [
        (Method::FloorDevicesList, None),
        (Method::FloorDevicesRemove, Some(named.clone())),
        (Method::FloorDevicesKeep, Some(named)),
    ] {
        let unproven = rpc(&settings, method, params.clone(), None).expect_err("unproven");
        assert_eq!(unproven.code, unauthorized, "{method:?}");
        column.set(PeerProvenance::Agent(agent));
        let as_agent = rpc(
            &settings,
            method,
            params.clone(),
            Some(credential_of(&settings)),
        )
        .expect_err("an agent");
        assert_eq!(as_agent.code, unauthorized, "{method:?}");
        column.set(PeerProvenance::Unresolvable);
        assert!(
            rpc(&settings, method, params, Some(credential_of(&settings))).is_ok(),
            "control: the operator is answered"
        );
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A second browser signing in tells an open page, and leaves its stream open.
#[test]
fn a_new_sign_in_reaches_an_open_stream_and_leaves_it_open() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let (settings, daemon, host, origin) = device_boot(&fixture, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("probe");
    let mut running = common::start(daemon);
    let page = exchange(addr, &host, &origin, &clock, &issue(&settings).bootstrap);

    let (mut stream, upgrade) = raw_upgrade(
        addr,
        &host,
        &origin,
        Some(&page.cookie),
        &stream_protocols(&page),
    );
    assert!(upgrade.starts_with("HTTP/1.1 101"), "{upgrade:?}");
    stream
        .write_all(&masked_text(br#"{"type":"floor.subscribe"}"#))
        .expect("subscribe");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&server_text(&mut stream)).expect("snapshot JSON");
    assert_eq!(snapshot["type"], "floor.snapshot", "{snapshot}");

    let _ = exchange(addr, &host, &origin, &clock, &issue(&settings).bootstrap);

    let mut seen = Vec::new();
    for _ in 0..4 {
        let frame: serde_json::Value =
            serde_json::from_slice(&server_text(&mut stream)).expect("frame JSON");
        let kind = frame["type"].as_str().unwrap_or_default().to_owned();
        seen.push(kind.clone());
        if kind == "floor.device.enrolled" {
            break;
        }
    }
    assert_eq!(
        seen.last().map(String::as_str),
        Some("floor.device.enrolled"),
        "{seen:?}"
    );
    let finishing_http = probe
        .hold_connection_slot_for_test()
        .expect("HTTP worker slot");
    let mut ping = masked_text(b"still open");
    ping[0] = 0x89;
    stream.write_all(&ping).expect("ping the subscribed stream");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        stream
            .set_read_timeout(Some(
                deadline.saturating_duration_since(std::time::Instant::now()),
            ))
            .expect("pong deadline");
        let (opcode, payload) = server_frame(&mut stream);
        if opcode == 0x0a {
            assert_eq!(payload, b"still open");
            break;
        }
        assert_eq!(opcode, 1, "only queued text may precede the pong");
    }
    drop(finishing_http);
    wait_for_live(&probe, 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A removed device's next request says why, and its open stream closes.
#[test]
fn a_removed_device_is_refused_with_its_reason_and_its_stream_closes() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let (settings, daemon, host, origin) = device_boot(&fixture, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("probe");
    let mut running = common::start(daemon);
    let page = exchange(addr, &host, &origin, &clock, &issue(&settings).bootstrap);
    let other = exchange(addr, &host, &origin, &clock, &issue(&settings).bootstrap);

    let (mut stream, upgrade) = raw_upgrade(
        addr,
        &host,
        &origin,
        Some(&page.cookie),
        &stream_protocols(&page),
    );
    assert!(upgrade.starts_with("HTTP/1.1 101"), "{upgrade:?}");
    wait_for_live(&probe, 1);

    let listed = post_json(addr, "/floor/devices", &host, &origin, &other, "{}");
    assert!(listed.0.starts_with("HTTP/1.1 200 OK\r\n"), "{}", listed.0);
    let value: serde_json::Value = serde_json::from_str(&listed.1).expect("JSON");
    let devices = value["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d["this"] == true).count(), 1);
    let target = devices
        .iter()
        .find(|device| device["this"] == false)
        .expect("the other browser")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(
        target,
        page.browser.device().to_string(),
        "the page can compute its own id from its key"
    );
    let removed = post_json(
        addr,
        "/floor/devices/remove",
        &host,
        &origin,
        &other,
        &format!(r#"{{"device":"{target}"}}"#),
    );
    assert!(
        removed.0.starts_with("HTTP/1.1 200 OK\r\n"),
        "{}",
        removed.0
    );

    wait_for_live(&probe, 0);
    stream
        .set_read_timeout(Some(CLOSE_BOUND))
        .expect("read timeout");
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);

    let refused = post_json(addr, "/floor/devices", &host, &origin, &page, "{}");
    assert!(
        refused.0.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{}",
        refused.0
    );
    assert_eq!(refused.1, r#"{"reason":"removed","session":"gone"}"#);
    let check = page.signed("POST", "/floor/session/check", b"{}");
    let (head, body) = post_standing(
        addr,
        "/floor/session/check",
        &host,
        &origin,
        (Some(&page.cookie), Some(&check)),
    );
    assert!(head.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{head}");
    assert_eq!(body, r#"{"reason":"removed","session":"unknown"}"#);
    let (_, unsigned) = post_standing(
        addr,
        "/floor/session/check",
        &host,
        &origin,
        (Some(&page.cookie), None),
    );
    assert_eq!(
        unsigned, body,
        "the reason grants nothing, so a page that cannot sign still hears it"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The stream handshake entries a page offers: the stream's protocol and a fresh signature.
fn stream_protocols(page: &Page) -> String {
    format!(
        "dudes-floor-v6, {}",
        page.signed("GET", "/floor/stream", &[]).protocol_entries()
    )
}

fn post_json(
    addr: SocketAddr,
    path: &str,
    host: &str,
    origin: &str,
    page: &Page,
    body: &str,
) -> (String, String) {
    let signed = page.signed("POST", path, body.as_bytes());
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCookie: dudes_floor_session={}\r\n{}\r\n{body}",
        body.len(),
        page.cookie,
        signed.header_lines()
    );
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let (head, body) = response_parts(&response);
    (head.to_owned(), String::from_utf8_lossy(body).into_owned())
}

/// `dudes open` asks for a link that lives a minute and `dudes floor open` for one that lives ten
/// minutes, and a call with no purpose is a call to open.
#[test]
fn a_link_lives_for_its_purpose() {
    use dudes_rpc::auth::PeerProvenance;

    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(START_MS, 0));
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let (settings, daemon, host, origin) = device_boot(&fixture, &clock, &column);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let call = |params: Option<serde_json::Value>| {
        let issued = rpc(
            &settings,
            Method::FloorSessionIssue,
            params,
            Some(credential_of(&settings)),
        )
        .expect("a link");
        serde_json::from_value::<SessionIssueResult>(issued).expect("typed")
    };
    let open = call(None);
    let explicit = call(Some(serde_json::json!({ "purpose": "open" })));
    let printed = call(Some(serde_json::json!({ "purpose": "print" })));
    assert_eq!(
        (
            open.expires_in_s,
            explicit.expires_in_s,
            printed.expires_in_s
        ),
        (60, 60, 600)
    );
    let bad = rpc(
        &settings,
        Method::FloorSessionIssue,
        Some(serde_json::json!({ "purpose": "email" })),
        Some(credential_of(&settings)),
    )
    .expect_err("an unknown purpose");
    assert_eq!(bad.code, ErrorCode::InvalidParams.code());

    clock.advance(61_000);
    assert_empty_refusal(&raw_link(addr, &host, &origin, &clock, &open.bootstrap));
    assert_empty_refusal(&raw_link(addr, &host, &origin, &clock, &explicit.bootstrap));
    let _ = exchange(addr, &host, &origin, &clock, &printed.bootstrap);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
