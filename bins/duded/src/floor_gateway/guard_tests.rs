//! Every dynamic Floor route goes through the guard, and the guard refuses what it must.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::mpsc::{self, Receiver};

use dudes_proto::FloorPeerRefusal;
use dudes_supervisor::testing::FakeClock;
use dudes_types::AgentId;

use super::guard::{Admission, DYNAMIC_PATHS};
use super::*;
use crate::floor_peer::{AgentRoot, ProcessRef, SocketOwner, SocketOwnerLookup};
use crate::ports::Job;
use crate::settings::FloorControl;

const HOST: &str = "127.0.0.1:43123";
const SIGN_IN_BODY_TYPE: &str = "text/plain;charset=UTF-8";
const ME: u32 = 7;
const AGENT_ROOT: u32 = 900;

fn agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn owner(lineage: &[(u32, u32)]) -> SocketOwnerLookup {
    SocketOwnerLookup::Found(vec![SocketOwner {
        uid: ME,
        lineage: lineage
            .iter()
            .map(|&(pid, group)| ProcessRef { pid, group })
            .collect(),
    }])
}

#[derive(Debug)]
struct Roots(Vec<AgentRoot>);

impl AgentRoots for Roots {
    fn live(&self) -> Vec<AgentRoot> {
        self.0.clone()
    }
}

/// A gate whose connection is held by `lookup`, with one agent running at `AGENT_ROOT`.
fn gate(lookup: SocketOwnerLookup) -> PeerGate {
    PeerGate::with(
        Arc::new(Roots(vec![AgentRoot {
            pid: AGENT_ROOT,
            agent: Some(agent()),
        }])),
        Box::new(move |_, _| lookup.clone()),
        Box::new(Some),
        Some(ME),
        None,
    )
}

fn outside() -> PeerGate {
    gate(owner(&[(4000, 4000), (1, 1)]))
}

fn inside_an_agent() -> PeerGate {
    gate(owner(&[(4100, 4100), (AGENT_ROOT, AGENT_ROOT), (1, 1)]))
}

struct Signed {
    sessions: FloorSessions,
    cookie: String,
    secret: String,
}

fn signed_in() -> Signed {
    let sessions = FloorSessions::new(
        HOST.parse().expect("loopback address"),
        Arc::new(FakeClock::frozen(0)),
    );
    let (cookie, secret) = sign_in(&sessions);
    Signed {
        sessions,
        cookie,
        secret,
    }
}

fn sign_in(sessions: &FloorSessions) -> (String, String) {
    let exchange = sessions
        .exchange(
            HOST,
            &format!("http://{HOST}"),
            sessions.issue().expect("a bootstrap").bootstrap.as_bytes(),
        )
        .expect("a session");
    (exchange.cookie, exchange.csrf)
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Status(u16),
    Upgraded,
}

/// What came of one request, and the jobs the handlers it reached would have run: every job except
/// the audit record for a refusal.
struct Driven {
    outcome: Outcome,
    reached_a_handler: usize,
    audit: Vec<(&'static str, Option<AgentId>, FloorPeerRefusal)>,
}

/// Every switch on whose reads this test file builds; the Git and diff reads are not built here.
fn all_on() -> Switches {
    Switches {
        answer: true,
        hire: true,
        control: FloorControl::On {
            goal: true,
            retire: true,
            renew: true,
        },
        recovery: true,
        board: true,
        terminal: true,
        git: false,
        diff: false,
        project: true,
    }
}

fn drive(request: &[u8], sessions: &FloorSessions, peer: &PeerGate) -> Driven {
    let assets = BTreeMap::new();
    let memory = MemoryReads::new();
    let board = TicketLimits::new();
    let switches = SharedSwitches::new(all_on());
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        terminal_csp: false,
        git: None,
        diff: None,
        board: &board,
        memory: &memory,
        peer,
    };
    let (jobs, received): (_, Receiver<Job>) = mpsc::channel();
    let handle = Handle::new(jobs);
    let clock = FakeClock::frozen(0);
    let guard = Guard {
        catalog,
        sessions,
        handle: &handle,
        clock: &clock,
    };
    let mut input = Input {
        bytes: Cursor::new(request.to_vec()),
    };
    let action = read_request(&mut input, guard, &AtomicU8::new(READY))
        .expect("the request is read")
        .expect("the request gets an answer");
    let outcome = match action {
        ConnectionAction::Response(response) => Outcome::Status(response.code),
        ConnectionAction::Upgrade(_) => Outcome::Upgraded,
    };
    let mut reached_a_handler = 0;
    let mut audit = Vec::new();
    for job in received.try_iter() {
        match job {
            Job::FloorPeerRefused {
                route,
                agent,
                reason,
            } => audit.push((route, agent, reason)),
            _ => reached_a_handler += 1,
        }
    }
    Driven {
        outcome,
        reached_a_handler,
        audit,
    }
}

struct Input {
    bytes: Cursor<Vec<u8>>,
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(buffer)
    }
}

impl RequestReader for Input {
    fn set_request_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn endpoints(&self) -> Option<Endpoints> {
        Some((
            "127.0.0.1:50000".parse().expect("a client address"),
            HOST.parse().expect("the listener address"),
        ))
    }
}

/// A POST with exactly the headers named; `None` leaves one out.
fn post(
    path: &str,
    cookie: Option<&str>,
    secret: Option<&str>,
    content_type: &str,
    length: usize,
    body: &[u8],
) -> Vec<u8> {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {HOST}\r\nOrigin: http://{HOST}\r\nContent-Type: {content_type}\r\n"
    );
    if let Some(cookie) = cookie {
        let _ = write!(head, "Cookie: {SESSION_COOKIE_NAME}={cookie}\r\n");
    }
    if let Some(secret) = secret {
        let _ = write!(head, "X-Dudes-CSRF: {secret}\r\n");
    }
    let _ = write!(head, "Content-Length: {length}\r\n\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn json_post(path: &str, cookie: Option<&str>, secret: Option<&str>, body: &[u8]) -> Vec<u8> {
    post(path, cookie, secret, "application/json", body.len(), body)
}

fn upgrade(path: &str, protocol: &str, cookie: Option<&str>) -> Vec<u8> {
    let cookie = cookie.map_or_else(String::new, |cookie| {
        format!("Cookie: {SESSION_COOKIE_NAME}={cookie}\r\n")
    });
    format!(
        "GET {path} HTTP/1.1\r\nHost: {HOST}\r\nOrigin: http://{HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\n{cookie}\r\n"
    )
    .into_bytes()
}

fn stream_protocols(secret: &str) -> String {
    format!("{FLOOR_PROTOCOL}, {CSRF_PROTOCOL_PREFIX}{secret}")
}

fn terminal_protocols(secret: &str) -> String {
    format!(
        "{}, {CSRF_PROTOCOL_PREFIX}{secret}",
        crate::floor_terminal::TERMINAL_PROTOCOL
    )
}

fn everything_on() -> (BTreeMap<String, Asset>, MemoryReads, TicketLimits) {
    (BTreeMap::new(), MemoryReads::new(), TicketLimits::new())
}

/// The paths a gateway source file names as routes, read from the source itself.
fn paths_named_in(source: &str, marker: &str) -> BTreeSet<String> {
    source
        .lines()
        .map(str::trim_start)
        .filter(|line| line.contains(marker))
        .filter_map(|line| {
            let start = line.find("\"/floor/")? + 1;
            let end = line[start..].find('"')? + start;
            Some(line[start..end].to_owned())
        })
        .collect()
}

#[test]
fn the_route_list_is_every_path_the_gateway_names() {
    let gateway = include_str!("../floor_gateway.rs");
    let tickets = include_str!("../floor_ticket.rs");
    let mut named = paths_named_in(gateway, "_PATH: &str = \"/floor/");
    named.extend(paths_named_in(tickets, "=> Self::"));
    named.remove("/floor/");
    let listed: BTreeSet<String> = DYNAMIC_PATHS
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert_eq!(
        named, listed,
        "a route the gateway names must be in DYNAMIC_PATHS, where the guard and these tests see it"
    );
    assert_eq!(listed.len(), DYNAMIC_PATHS.len(), "a path is listed twice");
    assert!(
        listed.len() >= 27,
        "the scan found too few routes to mean anything"
    );
}

#[test]
fn every_listed_path_is_a_route_when_its_switch_is_on_and_only_git_and_diff_are_off_here() {
    let (assets, memory, board) = everything_on();
    let switches = SharedSwitches::new(all_on());
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        terminal_csp: false,
        git: None,
        diff: None,
        board: &board,
        memory: &memory,
        peer: &outside(),
    };
    for path in DYNAMIC_PATHS {
        let method = if matches!(*path, STREAM_PATH | TERMINAL_PATH) {
            "GET"
        } else {
            "POST"
        };
        let routed = Route::of(method, path, catalog).is_some();
        assert_eq!(
            routed,
            !matches!(*path, GIT_PATH | DIFF_PATH),
            "{method} {path}"
        );
        assert!(
            Route::of(if method == "GET" { "POST" } else { "GET" }, path, catalog).is_none(),
            "{path} answers only {method}"
        );
    }
    let off = SharedSwitches::new(Switches {
        answer: false,
        control: FloorControl::Off,
        recovery: false,
        terminal: false,
        board: false,
        ..all_on()
    });
    let switched_off = Catalog {
        switches: &off,
        ..catalog
    };
    for path in [
        ANSWER_PATH,
        PAUSE_PATH,
        GOAL_PATH,
        RETIRE_PATH,
        INSPECT_RECOVERY_PATH,
        "/floor/ticket",
    ] {
        assert!(Route::of("POST", path, switched_off).is_none(), "{path}");
    }
    assert!(Route::of("GET", TERMINAL_PATH, switched_off).is_none());
}

fn max_body_of(path: &str) -> usize {
    let (assets, memory, board) = everything_on();
    let switches = SharedSwitches::new(all_on());
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        terminal_csp: false,
        git: None,
        diff: None,
        board: &board,
        memory: &memory,
        peer: &outside(),
    };
    Route::of("POST", path, catalog)
        .and_then(Route::action)
        .map(ActionRoute::max_body)
        .expect("a listed POST route")
}

#[test]
fn no_post_route_acts_without_a_clean_session_secret_body_and_peer() {
    let signed = signed_in();
    let other = signed_in();
    let (cookie, secret) = (signed.cookie.as_str(), signed.secret.as_str());
    let body = br#"{"a":1}"#;
    let routes: Vec<&str> = DYNAMIC_PATHS
        .iter()
        .copied()
        .filter(|path| !matches!(*path, STREAM_PATH | TERMINAL_PATH | SIGN_IN_PATH))
        .filter(|path| !matches!(*path, SESSION_CHECK_PATH | SESSION_END_PATH))
        .filter(|path| !matches!(*path, GIT_PATH | DIFF_PATH))
        .collect();
    assert!(routes.len() >= 22);
    for path in routes {
        let max = max_body_of(path);
        let too_big = post(
            path,
            Some(cookie),
            Some(secret),
            "application/json",
            max + 1,
            &[],
        );
        let cases: Vec<(&str, Vec<u8>, u16)> = vec![
            ("no cookie", json_post(path, None, Some(secret), body), 403),
            ("no secret", json_post(path, Some(cookie), None, body), 403),
            (
                "a wrong secret",
                json_post(path, Some(cookie), Some(&"A".repeat(43)), body),
                403,
            ),
            (
                "another session's secret",
                json_post(path, Some(cookie), Some(&other.secret), body),
                403,
            ),
            (
                "the wrong content type",
                post(
                    path,
                    Some(cookie),
                    Some(secret),
                    "text/plain",
                    body.len(),
                    body,
                ),
                400,
            ),
            ("a body over the cap", too_big, 413),
            (
                "a NUL byte",
                json_post(path, Some(cookie), Some(secret), b"{\"a\":\"\0\"}"),
                400,
            ),
            (
                "invalid UTF-8",
                json_post(path, Some(cookie), Some(secret), b"{\"a\":\"\xff\"}"),
                400,
            ),
        ];
        for (what, request, status) in cases {
            let driven = drive(&request, &signed.sessions, &outside());
            assert_eq!(
                driven.outcome,
                Outcome::Status(status),
                "{path} with {what}"
            );
            assert_eq!(driven.reached_a_handler, 0, "{path} with {what}");
        }

        let request = json_post(path, Some(cookie), Some(secret), body);
        let from_an_agent = drive(&request, &signed.sessions, &inside_an_agent());
        assert_eq!(from_an_agent.outcome, Outcome::Status(403), "{path}");
        assert_eq!(from_an_agent.reached_a_handler, 0, "{path}");
        assert_eq!(
            from_an_agent.audit,
            vec![(path_of(path), Some(agent()), FloorPeerRefusal::Agent)],
            "{path}"
        );
        let nobody = drive(
            &request,
            &signed.sessions,
            &gate(SocketOwnerLookup::NotFound),
        );
        assert_eq!(nobody.outcome, Outcome::Status(403), "{path}");
        assert_eq!(nobody.reached_a_handler, 0, "{path}");
    }
}

#[test]
fn the_session_routes_say_unknown_only_to_a_clean_peer_and_end_only_for_the_session_itself() {
    let signed = signed_in();
    let (cookie, secret) = (signed.cookie.as_str(), signed.secret.as_str());
    let check = |sessions: &FloorSessions| {
        let request = json_post(SESSION_CHECK_PATH, Some(cookie), Some(secret), b"{}");
        drive(&request, sessions, &outside()).outcome
    };
    let end = json_post(SESSION_END_PATH, Some(cookie), Some(secret), b"{}");

    let from_an_agent = drive(&end, &signed.sessions, &inside_an_agent());
    assert_eq!(from_an_agent.outcome, Outcome::Status(403));
    assert_eq!(
        from_an_agent.audit,
        vec![(
            path_of(SESSION_END_PATH),
            Some(agent()),
            FloorPeerRefusal::Agent
        )]
    );
    assert_eq!(
        check(&signed.sessions),
        Outcome::Status(200),
        "an agent cannot sign the user out"
    );

    for (what, request) in [
        (
            "no credentials",
            json_post(SESSION_END_PATH, None, None, b"{}"),
        ),
        (
            "a wrong secret",
            json_post(SESSION_END_PATH, Some(cookie), Some(&"A".repeat(43)), b"{}"),
        ),
    ] {
        let driven = drive(&request, &signed.sessions, &outside());
        assert_eq!(driven.outcome, Outcome::Status(401), "{what}");
    }
    assert_eq!(
        check(&signed.sessions),
        Outcome::Status(200),
        "an unknown request ends nothing"
    );

    assert_eq!(
        drive(&end, &signed.sessions, &outside()).outcome,
        Outcome::Status(200)
    );
    assert_eq!(check(&signed.sessions), Outcome::Status(401));
}

fn path_of(path: &str) -> &'static str {
    DYNAMIC_PATHS
        .iter()
        .copied()
        .find(|known| *known == path)
        .expect("a listed path")
}

#[test]
fn the_stream_and_the_terminal_need_the_cookie_the_secret_and_a_clean_peer() {
    let signed = signed_in();
    let other = signed_in();
    let cookie = signed.cookie.as_str();
    for (path, protocols) in [
        (STREAM_PATH, stream_protocols as fn(&str) -> String),
        (TERMINAL_PATH, terminal_protocols),
    ] {
        let good = upgrade(path, &protocols(&signed.secret), Some(cookie));
        let refused = [
            ("no cookie", upgrade(path, &protocols(&signed.secret), None)),
            (
                "the cookie alone",
                upgrade(path, &stream_protocols_without_secret(path), Some(cookie)),
            ),
            (
                "a wrong secret",
                upgrade(path, &protocols(&"A".repeat(43)), Some(cookie)),
            ),
            (
                "another session's secret",
                upgrade(path, &protocols(&other.secret), Some(cookie)),
            ),
        ];
        for (what, request) in refused {
            let driven = drive(&request, &signed.sessions, &outside());
            assert_eq!(driven.outcome, Outcome::Status(403), "{path} with {what}");
        }
        let agent_driven = drive(&good, &signed.sessions, &inside_an_agent());
        assert_eq!(agent_driven.outcome, Outcome::Status(403), "{path}");
        assert_eq!(
            agent_driven.audit,
            vec![(path_of(path), Some(agent()), FloorPeerRefusal::Agent)]
        );
        let nobody = drive(&good, &signed.sessions, &gate(SocketOwnerLookup::NotFound));
        assert_eq!(nobody.outcome, Outcome::Status(403), "{path}");
        assert_eq!(
            drive(&good, &signed.sessions, &outside()).outcome,
            Outcome::Upgraded,
            "{path} from outside every agent"
        );
    }
}

fn stream_protocols_without_secret(path: &str) -> String {
    let protocol = if path == STREAM_PATH {
        FLOOR_PROTOCOL
    } else {
        crate::floor_terminal::TERMINAL_PROTOCOL
    };
    format!("{protocol}, {CSRF_PROTOCOL_PREFIX}")
}

#[test]
fn an_agent_cannot_spend_the_sign_in_link_and_the_owner_still_can() {
    let sessions = FloorSessions::new(
        HOST.parse().expect("loopback address"),
        Arc::new(FakeClock::frozen(0)),
    );
    let bootstrap = sessions.issue().expect("a bootstrap").bootstrap;
    let request = post(
        SIGN_IN_PATH,
        None,
        None,
        SIGN_IN_BODY_TYPE,
        bootstrap.len(),
        bootstrap.as_bytes(),
    );
    let from_an_agent = drive(&request, &sessions, &inside_an_agent());
    assert_eq!(from_an_agent.outcome, Outcome::Status(403));
    assert_eq!(from_an_agent.audit.len(), 1);
    let from_the_owner = drive(&request, &sessions, &outside());
    assert_eq!(
        from_the_owner.outcome,
        Outcome::Status(200),
        "the refused attempt left the link unspent"
    );
}

#[test]
fn the_old_cookie_only_recovery_route_is_gone() {
    let signed = signed_in();
    let request = format!(
        "POST /floor/session/recover HTTP/1.1\r\nHost: {HOST}\r\nOrigin: http://{HOST}\r\nCookie: {SESSION_COOKIE_NAME}={}\r\nContent-Length: 0\r\n\r\n",
        signed.cookie
    );
    let driven = drive(request.as_bytes(), &signed.sessions, &outside());
    assert!(
        matches!(driven.outcome, Outcome::Status(code) if !(200..400).contains(&code)),
        "{:?}",
        driven.outcome
    );
}

#[test]
fn a_stopping_gateway_does_not_call_a_cleared_session_unknown() {
    let signed = signed_in();
    let request = json_post(
        SESSION_CHECK_PATH,
        Some(&signed.cookie),
        Some(&signed.secret),
        b"{}",
    );
    assert_eq!(
        drive(&request, &signed.sessions, &outside()).outcome,
        Outcome::Status(200)
    );

    signed.sessions.invalidate();

    assert_eq!(
        drive(&request, &signed.sessions, &outside()).outcome,
        Outcome::Status(503),
        "a page that asks while the daemon stops must keep its secret"
    );
}

#[test]
fn where_the_owner_cannot_be_looked_up_sign_out_is_refused_and_the_check_is_served() {
    let signed = signed_in();
    let view_only = gate(SocketOwnerLookup::Unsupported);
    let request = |path| json_post(path, Some(&signed.cookie), Some(&signed.secret), b"{}");
    assert_eq!(
        drive(&request(SESSION_END_PATH), &signed.sessions, &view_only).outcome,
        Outcome::Status(403)
    );
    assert_eq!(
        drive(&request(SESSION_CHECK_PATH), &signed.sessions, &view_only).outcome,
        Outcome::Status(200)
    );
}

#[test]
fn where_the_owner_cannot_be_looked_up_the_floor_shows_and_does_not_act() {
    let signed = signed_in();
    let (cookie, secret) = (signed.cookie.as_str(), signed.secret.as_str());
    let view_only = gate(SocketOwnerLookup::Unsupported);
    let body = br#"{"a":1}"#;
    assert_eq!(
        drive(
            &upgrade(STREAM_PATH, &stream_protocols(secret), Some(cookie)),
            &signed.sessions,
            &view_only
        )
        .outcome,
        Outcome::Upgraded,
        "the stream opens, to look"
    );
    for path in [
        ANSWER_PATH,
        PAUSE_PATH,
        ONBOARD_PATH,
        SETTINGS_PATH,
        PROJECT_ADD_PATH,
        "/floor/ticket",
        CHAT_SEND_PATH,
    ] {
        let driven = drive(
            &json_post(path, Some(cookie), Some(secret), body),
            &signed.sessions,
            &view_only,
        );
        assert_eq!(driven.outcome, Outcome::Status(403), "{path}");
        assert_eq!(driven.reached_a_handler, 0, "{path}");
    }
    assert_eq!(
        drive(
            &upgrade(TERMINAL_PATH, &terminal_protocols(secret), Some(cookie)),
            &signed.sessions,
            &view_only
        )
        .outcome,
        Outcome::Status(403),
        "a terminal is acting"
    );
}

#[test]
fn refusals_for_one_agent_reach_the_journal_once_a_minute() {
    let signed = signed_in();
    let request = json_post(
        ANSWER_PATH,
        Some(&signed.cookie),
        Some(&signed.secret),
        b"{}",
    );
    let gate = inside_an_agent();
    let first = drive(&request, &signed.sessions, &gate);
    let second = drive(&request, &signed.sessions, &gate);
    assert_eq!(first.outcome, Outcome::Status(403));
    assert_eq!(second.outcome, Outcome::Status(403));
    assert_eq!(first.audit.len(), 1);
    assert!(
        second.audit.is_empty(),
        "the second refusal is within the minute"
    );
}

#[test]
fn the_guard_admits_a_clean_request_from_outside_every_agent() {
    let signed = signed_in();
    let assets = BTreeMap::new();
    let memory = MemoryReads::new();
    let peer = outside();
    let board = TicketLimits::new();
    let switches = SharedSwitches::new(Switches {
        control: FloorControl::Off,
        recovery: false,
        terminal: false,
        board: false,
        ..all_on()
    });
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        terminal_csp: false,
        git: None,
        diff: None,
        board: &board,
        memory: &memory,
        peer: &peer,
    };
    let handle = Handle::new(mpsc::channel().0);
    let clock = FakeClock::frozen(0);
    let guard = Guard {
        catalog,
        sessions: &signed.sessions,
        handle: &handle,
        clock: &clock,
    };
    let route = Route::of("POST", ANSWER_PATH, catalog).expect("answer is on");
    let head = Head {
        host: HOST.to_owned(),
        origin: format!("http://{HOST}"),
        credentials: Some(Credentials {
            cookie: signed.cookie.clone(),
            secret: signed.secret.clone(),
        }),
        content_length: 2,
        accept: String::new(),
    };
    let endpoints = Some(("127.0.0.1:50000".parse().unwrap(), HOST.parse().unwrap()));
    let body = || Ok(b"{}".to_vec());
    assert!(matches!(
        guard.admit(route, &head, body, endpoints),
        Ok(Admission {
            admitted: Admitted::Session(_),
            ..
        })
    ));
    assert!(
        guard.admit(route, &head, body, None).is_err(),
        "a transport that cannot name its endpoints is not admitted"
    );
}
