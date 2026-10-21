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
use crate::engine_login::{EngineLogins, Refusal};
use crate::floor_browser::{Browser, Signed as SignedValues};
use crate::floor_peer::{AgentRoot, ProcessRef, SocketOwner, SocketOwnerLookup};
use crate::floor_session::Presentation;
use crate::ports::ClosedPtyHost;
use crate::ports::Job;
use crate::settings::ENGINE_LOGIN_TIMEOUT;
use crate::settings::FloorControl;

const HOST: &str = "127.0.0.1:43123";
const SIGN_IN_BODY_TYPE: &str = "application/json";
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

/// Sign-ins that start nothing: these tests look at admission, not at the engine.
fn engine_logins() -> EngineLogins {
    EngineLogins::new(
        |_, _| Err(Refusal::NotInstalled),
        |_| {},
        Box::new(ClosedPtyHost),
        ENGINE_LOGIN_TIMEOUT,
    )
}

fn outside() -> PeerGate {
    gate(owner(&[(4000, 4000), (1, 1)]))
}

fn inside_an_agent() -> PeerGate {
    gate(owner(&[(4100, 4100), (AGENT_ROOT, AGENT_ROOT), (1, 1)]))
}

/// A signed-in browser: the table it joined, its cookie, and the key that signs for it. Every
/// harness clock is frozen at zero, so every request is signed for the time zero.
struct Page {
    sessions: FloorSessions,
    cookie: String,
    browser: Browser,
}

impl Page {
    fn origin() -> String {
        format!("http://{HOST}")
    }

    /// The values this browser signs `method path` with, over `body`.
    fn sign(&self, method: &str, path: &str, body: &[u8]) -> SignedValues {
        self.browser.sign(method, path, &Self::origin(), body, 0)
    }

    /// A signed JSON POST.
    fn json(&self, path: &str, body: &[u8]) -> Vec<u8> {
        json_post(
            path,
            Some(&self.cookie),
            Some(&self.sign("POST", path, body)),
            body,
        )
    }

    /// A signed socket handshake offering `protocol` first.
    fn socket(&self, path: &str, protocol: &str) -> Vec<u8> {
        let signed = self.sign("GET", path, &[]);
        upgrade(
            path,
            &format!("{protocol}, {}", signed.protocol_entries()),
            Some(&self.cookie),
        )
    }
}

fn signed_in() -> Page {
    let sessions = FloorSessions::new(
        HOST.parse().expect("loopback address"),
        Arc::new(FakeClock::frozen(0)),
    );
    join(sessions)
}

fn join(sessions: FloorSessions) -> Page {
    let link = sessions.issue().expect("a bootstrap");
    let joined = sessions
        .join(HOST, &Page::origin(), &link.bootstrap)
        .expect("a session");
    Page {
        sessions,
        cookie: joined.exchange.cookie.clone(),
        browser: joined.browser,
    }
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
    body: Vec<u8>,
    /// The `Cache-Control` the answer carried, when it was an answer and not an upgrade.
    cache: Option<&'static str>,
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
    let logins = engine_logins();
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        git: None,
        diff: None,
        logins: Some(&logins),
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
    let (outcome, body, cache) = match action {
        ConnectionAction::Response(response) => (
            Outcome::Status(response.code),
            response.body.into_owned(),
            Some(response.cache),
        ),
        ConnectionAction::Upgrade(_) => (Outcome::Upgraded, Vec::new(), None),
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
        body,
        cache,
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
    signed: Option<&SignedValues>,
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
    if let Some(signed) = signed {
        head.push_str(&signed.header_lines());
    }
    let _ = write!(head, "Content-Length: {length}\r\n\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn json_post(
    path: &str,
    cookie: Option<&str>,
    signed: Option<&SignedValues>,
    body: &[u8],
) -> Vec<u8> {
    post(path, cookie, signed, "application/json", body.len(), body)
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
    let logins = engine_logins();
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        git: None,
        diff: None,
        logins: Some(&logins),
        board: &board,
        memory: &memory,
        peer: &outside(),
    };
    for path in DYNAMIC_PATHS {
        let method = if matches!(*path, STREAM_PATH | TERMINAL_PATH | ENGINE_LOGIN_PATH) {
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
    assert!(
        Route::of("GET", ENGINE_LOGIN_PATH, switched_off).is_some(),
        "signing in follows hiring, not the terminal"
    );
    let hiring_off = SharedSwitches::new(Switches {
        hire: false,
        ..all_on()
    });
    assert!(
        Route::of(
            "GET",
            ENGINE_LOGIN_PATH,
            Catalog {
                switches: &hiring_off,
                ..catalog
            }
        )
        .is_none()
    );
    assert!(
        Route::of(
            "GET",
            ENGINE_LOGIN_PATH,
            Catalog {
                logins: None,
                ..catalog
            }
        )
        .is_none()
    );
}

fn max_body_of(path: &str) -> usize {
    let (assets, memory, board) = everything_on();
    let switches = SharedSwitches::new(all_on());
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        git: None,
        diff: None,
        logins: None,
        board: &board,
        memory: &memory,
        peer: &outside(),
    };
    Route::of("POST", path, catalog)
        .and_then(Route::action)
        .map(ActionRoute::max_body)
        .expect("a listed POST route")
}

/// A request head the way the parsers would read it, for a test that calls the guard directly.
fn head_of(page: &Page, path: &str, body: &[u8], signed: &SignedValues) -> Head {
    Head {
        host: HOST.to_owned(),
        origin: Page::origin(),
        method: "POST".to_owned(),
        target: path.to_owned(),
        cookie: Some(page.cookie.clone()),
        signed: Some(Signed {
            origin: signed.origin.clone(),
            time: signed.time.clone(),
            request: signed.request.clone(),
            signature: signed.signature.clone(),
        }),
        content_length: body.len(),
        accept: String::new(),
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table of refusals walked over every route"
)]
fn no_post_route_acts_without_a_valid_signature_a_clean_body_and_peer() {
    let page = signed_in();
    let other = signed_in();
    let cookie = page.cookie.as_str();
    let body = br#"{"a":1}"#;
    let routes: Vec<&str> = DYNAMIC_PATHS
        .iter()
        .copied()
        .filter(|path| {
            !matches!(
                *path,
                STREAM_PATH | TERMINAL_PATH | ENGINE_LOGIN_PATH | SIGN_IN_PATH
            )
        })
        .filter(|path| !matches!(*path, SESSION_CHECK_PATH | SESSION_END_PATH))
        .filter(|path| !matches!(*path, GIT_PATH | DIFF_PATH))
        .collect();
    assert!(routes.len() >= 22);
    for path in routes {
        let max = max_body_of(path);
        let sign = |body: &[u8]| page.sign("POST", path, body);
        let too_big = post(
            path,
            Some(cookie),
            Some(&sign(&[])),
            "application/json",
            max + 1,
            &[],
        );
        let elsewhere = page
            .browser
            .sign("POST", path, "http://127.0.0.1:1", body, 0);
        let wrong_origin_header = json_post(path, Some(cookie), Some(&elsewhere), body);
        let cases: Vec<(&str, Vec<u8>, u16)> = vec![
            (
                "no cookie",
                json_post(path, None, Some(&sign(body)), body),
                403,
            ),
            (
                "no signature",
                json_post(path, Some(cookie), None, body),
                401,
            ),
            (
                "another browser's signature",
                json_post(
                    path,
                    Some(cookie),
                    Some(&other.sign("POST", path, body)),
                    body,
                ),
                401,
            ),
            (
                "a signature made for another origin",
                wrong_origin_header,
                401,
            ),
            (
                "a signature made for another path",
                json_post(
                    path,
                    Some(cookie),
                    Some(&page.sign("POST", "/floor/elsewhere", body)),
                    body,
                ),
                401,
            ),
            (
                "a signature over another body",
                json_post(path, Some(cookie), Some(&sign(b"{}")), body),
                401,
            ),
            (
                "a time over a minute ahead",
                json_post(
                    path,
                    Some(cookie),
                    Some(
                        &page
                            .browser
                            .sign("POST", path, &Page::origin(), body, 60_001),
                    ),
                    body,
                ),
                401,
            ),
            (
                "the wrong content type",
                post(
                    path,
                    Some(cookie),
                    Some(&sign(body)),
                    "text/plain",
                    body.len(),
                    body,
                ),
                400,
            ),
            ("a body over the cap", too_big, 413),
            (
                "a NUL byte",
                json_post(
                    path,
                    Some(cookie),
                    Some(&sign(b"{\"a\":\"\0\"}")),
                    b"{\"a\":\"\0\"}",
                ),
                400,
            ),
            (
                "invalid UTF-8",
                json_post(
                    path,
                    Some(cookie),
                    Some(&sign(b"{\"a\":\"\xff\"}")),
                    b"{\"a\":\"\xff\"}",
                ),
                400,
            ),
        ];
        for (what, request, status) in cases {
            let driven = drive(&request, &page.sessions, &outside());
            assert_eq!(
                driven.outcome,
                Outcome::Status(status),
                "{path} with {what}"
            );
            assert_eq!(driven.reached_a_handler, 0, "{path} with {what}");
            assert_eq!(
                driven.cache,
                Some("no-store"),
                "{path} with {what}: every dynamic answer is uncacheable"
            );
        }
        let unsigned = drive(
            &json_post(path, Some(cookie), None, body),
            &page.sessions,
            &outside(),
        );
        assert_eq!(
            unsigned.body, br#"{"signature":"refused"}"#,
            "{path}: a refused signature says so, so the page can retry once"
        );

        let request = page.json(path, body);
        let from_an_agent = drive(&request, &page.sessions, &inside_an_agent());
        assert_eq!(from_an_agent.outcome, Outcome::Status(403), "{path}");
        assert_eq!(from_an_agent.reached_a_handler, 0, "{path}");
        assert_eq!(
            from_an_agent.audit,
            vec![(path_of(path), Some(agent()), FloorPeerRefusal::Agent)],
            "{path}"
        );
        let nobody = drive(&request, &page.sessions, &gate(SocketOwnerLookup::NotFound));
        assert_eq!(nobody.outcome, Outcome::Status(403), "{path}");
        assert_eq!(nobody.reached_a_handler, 0, "{path}");
    }
}

#[test]
fn the_cookie_alone_opens_nothing() {
    let page = signed_in();
    for path in [
        ANSWER_PATH,
        SETTINGS_PATH,
        DEVICES_PATH,
        "/floor/ticket/read",
    ] {
        let driven = drive(
            &json_post(path, Some(&page.cookie), None, b"{}"),
            &page.sessions,
            &outside(),
        );
        assert_eq!(driven.outcome, Outcome::Status(401), "{path}");
        assert_eq!(driven.reached_a_handler, 0, "{path}");
    }
    let socket = upgrade(STREAM_PATH, FLOOR_PROTOCOL, Some(&page.cookie));
    assert_eq!(
        drive(&socket, &page.sessions, &outside()).outcome,
        Outcome::Status(401)
    );
}

#[test]
fn a_request_id_works_once_and_a_forged_request_does_not_spend_one() {
    let page = signed_in();
    let assets = BTreeMap::new();
    let memory = MemoryReads::new();
    let peer = outside();
    let board = TicketLimits::new();
    let switches = SharedSwitches::new(all_on());
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        git: None,
        diff: None,
        logins: None,
        board: &board,
        memory: &memory,
        peer: &peer,
    };
    let handle = Handle::new(mpsc::channel().0);
    let clock = FakeClock::frozen(0);
    let guard = Guard {
        catalog,
        sessions: &page.sessions,
        handle: &handle,
        clock: &clock,
    };
    let route = Route::of("POST", ANSWER_PATH, catalog).expect("answer is on");
    let endpoints = Some(("127.0.0.1:50000".parse().unwrap(), HOST.parse().unwrap()));
    let body = || Ok(b"{}".to_vec());
    let signed = page
        .browser
        .sign_with_id("POST", ANSWER_PATH, &Page::origin(), b"{}", 0, [7; 16]);
    let head = head_of(&page, ANSWER_PATH, b"{}", &signed);
    assert!(guard.admit(route, &head, body, endpoints).is_ok());
    let Err(refused) = guard.admit(route, &head, body, endpoints) else {
        panic!("a second delivery of one request id was admitted");
    };
    assert!(matches!(
        refused,
        Denied::Response(response) if response.code == 401
    ));

    let mut forged =
        page.browser
            .sign_with_id("POST", ANSWER_PATH, &Page::origin(), b"{}", 0, [8; 16]);
    forged.signature = other_signature(&page);
    let forged_head = head_of(&page, ANSWER_PATH, b"{}", &forged);
    assert!(guard.admit(route, &forged_head, body, endpoints).is_err());
    let honest = page
        .browser
        .sign_with_id("POST", ANSWER_PATH, &Page::origin(), b"{}", 0, [8; 16]);
    assert!(
        guard
            .admit(
                route,
                &head_of(&page, ANSWER_PATH, b"{}", &honest),
                body,
                endpoints
            )
            .is_ok(),
        "the forged request did not use up the id the honest one carries"
    );
}

/// A well-formed signature that is not this request's.
fn other_signature(page: &Page) -> String {
    page.sign("POST", "/floor/elsewhere", b"{}").signature
}

#[test]
fn the_session_routes_say_unknown_only_to_a_clean_peer_and_end_only_for_the_session_itself() {
    let page = signed_in();
    let check = |sessions: &FloorSessions| {
        let request = page.json(SESSION_CHECK_PATH, b"{}");
        drive(&request, sessions, &outside()).outcome
    };
    let end = page.json(SESSION_END_PATH, b"{}");

    let from_an_agent = drive(&end, &page.sessions, &inside_an_agent());
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
        check(&page.sessions),
        Outcome::Status(200),
        "an agent cannot sign the user out"
    );

    for (what, request, status) in [
        (
            "no cookie",
            json_post(SESSION_END_PATH, None, None, b"{}"),
            401,
        ),
        (
            "a cookie no device holds",
            json_post(
                SESSION_END_PATH,
                Some(&"A".repeat(43)),
                Some(&page.sign("POST", SESSION_END_PATH, b"{}")),
                b"{}",
            ),
            401,
        ),
        (
            "the cookie without a signature",
            json_post(SESSION_END_PATH, Some(&page.cookie), None, b"{}"),
            401,
        ),
    ] {
        let driven = drive(&request, &page.sessions, &outside());
        assert_eq!(driven.outcome, Outcome::Status(status), "{what}");
    }
    let unsigned = drive(
        &json_post(SESSION_END_PATH, Some(&page.cookie), None, b"{}"),
        &page.sessions,
        &outside(),
    );
    assert_eq!(unsigned.body, br#"{"signature":"refused"}"#);
    let unknown = drive(
        &json_post(SESSION_END_PATH, None, None, b"{}"),
        &page.sessions,
        &outside(),
    );
    assert_eq!(unknown.body, br#"{"session":"unknown"}"#);
    assert_eq!(
        check(&page.sessions),
        Outcome::Status(200),
        "an unknown request ends nothing"
    );

    assert_eq!(
        drive(&end, &page.sessions, &outside()).outcome,
        Outcome::Status(200)
    );
    assert_eq!(check(&page.sessions), Outcome::Status(401));
}

fn asset(bytes: &[u8], content_type: &'static str) -> Asset {
    Asset {
        bytes: bytes.to_vec(),
        content_type,
        cache: "no-store",
    }
}

#[test]
fn a_script_path_the_daemon_does_not_serve_is_a_404_and_never_the_page() {
    let page = signed_in();
    let assets = BTreeMap::from([
        (
            "/floor/".to_owned(),
            asset(b"<!doctype html>", "text/html; charset=utf-8"),
        ),
        (
            "/floor/assets/app-1.js".to_owned(),
            asset(b"console.log(1)", "text/javascript"),
        ),
    ]);
    let get = |path: &str| {
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: {HOST}\r\nOrigin: http://{HOST}\r\n\r\n");
        let memory = MemoryReads::new();
        let board = TicketLimits::new();
        let switches = SharedSwitches::new(all_on());
        let peer = outside();
        let catalog = Catalog {
            assets: &assets,
            manifest: &[],
            switches: &switches,
            git: None,
            diff: None,
            logins: None,
            board: &board,
            memory: &memory,
            peer: &peer,
        };
        let handle = Handle::new(mpsc::channel().0);
        let clock = FakeClock::frozen(0);
        let guard = Guard {
            catalog,
            sessions: &page.sessions,
            handle: &handle,
            clock: &clock,
        };
        let mut input = Input {
            bytes: Cursor::new(request.into_bytes()),
        };
        match read_request(&mut input, guard, &AtomicU8::new(READY))
            .expect("the request is read")
            .expect("the request gets an answer")
        {
            ConnectionAction::Response(response) => (response.code, response.body.into_owned()),
            ConnectionAction::Upgrade(_) => panic!("a GET without an upgrade offer upgraded"),
        }
    };
    assert_eq!(
        get("/floor/assets/app-1.js"),
        (200, b"console.log(1)".to_vec())
    );
    for path in [
        "/floor/sw.js",
        "/floor/service-worker.js",
        "/floor/assets/worker.js",
        "/floor/assets/app-2.js",
        "/floor/assets/chunk.mjs",
        "/floor/x/../sw.js",
        "/floor/sw.js?v=1",
    ] {
        let (code, body) = get(path);
        assert_eq!(code, 404, "{path}");
        assert!(body.is_empty(), "{path} did not fall back to the page");
    }
}

fn path_of(path: &str) -> &'static str {
    DYNAMIC_PATHS
        .iter()
        .copied()
        .find(|known| *known == path)
        .expect("a listed path")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table of refusals walked over every socket"
)]
fn the_sockets_need_the_cookie_a_valid_signature_and_a_clean_peer() {
    let page = signed_in();
    let other = signed_in();
    let cookie = page.cookie.as_str();
    for (path, protocol) in [
        (STREAM_PATH, FLOOR_PROTOCOL),
        (TERMINAL_PATH, crate::floor_terminal::TERMINAL_PROTOCOL),
        (ENGINE_LOGIN_PATH, crate::engine_login::PROTOCOL),
    ] {
        let signed = |time: i64, origin: &str| page.browser.sign("GET", path, origin, &[], time);
        let offering = |signed: &SignedValues| {
            upgrade(
                path,
                &format!("{protocol}, {}", signed.protocol_entries()),
                Some(cookie),
            )
        };
        let good = page.socket(path, protocol);
        let replayed = offering(&signed(0, &Page::origin()));
        let refused = [
            (
                "no cookie",
                upgrade(
                    path,
                    &format!(
                        "{protocol}, {}",
                        signed(0, &Page::origin()).protocol_entries()
                    ),
                    None,
                ),
                403,
            ),
            (
                "the cookie alone",
                upgrade(path, protocol, Some(cookie)),
                401,
            ),
            (
                "another browser's signature",
                upgrade(
                    path,
                    &format!(
                        "{protocol}, {}",
                        other.sign("GET", path, &[]).protocol_entries()
                    ),
                    Some(cookie),
                ),
                401,
            ),
            (
                "a signature made for another origin",
                offering(&signed(0, "http://127.0.0.1:1")),
                401,
            ),
            (
                "a time over a minute ahead",
                offering(&signed(60_001, &Page::origin())),
                401,
            ),
            (
                "the old subprotocol",
                upgrade(
                    path,
                    &format!("{protocol}, dudes-csrf.{}", "A".repeat(43)),
                    Some(cookie),
                ),
                403,
            ),
            (
                "the previous stream version",
                upgrade(
                    path,
                    &format!(
                        "dudes-floor-v3, {}",
                        signed(0, &Page::origin()).protocol_entries()
                    ),
                    Some(cookie),
                ),
                403,
            ),
        ];
        for (what, request, status) in refused {
            let driven = drive(&request, &page.sessions, &outside());
            assert_eq!(
                driven.outcome,
                Outcome::Status(status),
                "{path} with {what}"
            );
        }
        let agent_driven = drive(&good, &page.sessions, &inside_an_agent());
        assert_eq!(agent_driven.outcome, Outcome::Status(403), "{path}");
        assert_eq!(
            agent_driven.audit,
            vec![(path_of(path), Some(agent()), FloorPeerRefusal::Agent)]
        );
        let nobody = drive(&good, &page.sessions, &gate(SocketOwnerLookup::NotFound));
        assert_eq!(nobody.outcome, Outcome::Status(403), "{path}");
        assert_eq!(
            drive(&good, &page.sessions, &outside()).outcome,
            Outcome::Upgraded,
            "{path} from outside every agent"
        );
        assert_eq!(
            drive(&replayed, &page.sessions, &outside()).outcome,
            Outcome::Upgraded,
            "{path}: a fresh request id opens"
        );
        assert_eq!(
            drive(&replayed, &page.sessions, &outside()).outcome,
            Outcome::Status(401),
            "{path}: the same request id twice"
        );
    }
}

#[test]
fn an_agent_cannot_spend_the_sign_in_link_and_the_owner_still_can() {
    let sessions = FloorSessions::new(
        HOST.parse().expect("loopback address"),
        Arc::new(FakeClock::frozen(0)),
    );
    let bootstrap = sessions.issue().expect("a bootstrap").bootstrap;
    let body = Browser::new().enrolment(&bootstrap, "Chrome on macOS");
    let request = post(
        SIGN_IN_PATH,
        None,
        None,
        SIGN_IN_BODY_TYPE,
        body.len(),
        body.as_bytes(),
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
    let request = signed.json(SESSION_CHECK_PATH, b"{}");
    assert_eq!(
        drive(&request, &signed.sessions, &outside()).outcome,
        Outcome::Status(200)
    );

    signed.sessions.invalidate();

    assert_eq!(
        drive(&request, &signed.sessions, &outside()).outcome,
        Outcome::Status(503),
        "a page that asks while the daemon stops must keep its key"
    );
}

#[test]
fn where_the_owner_cannot_be_looked_up_sign_out_is_refused_and_the_check_is_served() {
    let signed = signed_in();
    let view_only = gate(SocketOwnerLookup::Unsupported);
    let request = |path| signed.json(path, b"{}");
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
    let view_only = gate(SocketOwnerLookup::Unsupported);
    let body = br#"{"a":1}"#;
    assert_eq!(
        drive(
            &signed.socket(STREAM_PATH, FLOOR_PROTOCOL),
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
        PROJECT_REMOVE_PATH,
        "/floor/ticket",
        CHAT_SEND_PATH,
    ] {
        let driven = drive(&signed.json(path, body), &signed.sessions, &view_only);
        assert_eq!(driven.outcome, Outcome::Status(403), "{path}");
        assert_eq!(driven.reached_a_handler, 0, "{path}");
    }
    assert_eq!(
        drive(
            &signed.socket(TERMINAL_PATH, crate::floor_terminal::TERMINAL_PROTOCOL),
            &signed.sessions,
            &view_only
        )
        .outcome,
        Outcome::Status(403),
        "a terminal is acting"
    );
    assert_eq!(
        drive(
            &signed.socket(ENGINE_LOGIN_PATH, crate::engine_login::PROTOCOL),
            &signed.sessions,
            &view_only
        )
        .outcome,
        Outcome::Status(403),
        "signing in is acting"
    );
}

#[test]
fn a_session_holds_one_sign_in_socket_at_a_time() {
    let signed = signed_in();
    let authorizer = signed.sessions.authorizer();
    let (host, origin) = (HOST, format!("http://{HOST}"));
    let open = || {
        let signed_values = signed
            .browser
            .sign("GET", ENGINE_LOGIN_PATH, &origin, &[], 0);
        let request =
            Presentation::new(host, &origin, &signed.cookie, "GET", ENGINE_LOGIN_PATH, &[]).signed(
                &signed_values.origin,
                &signed_values.time,
                &signed_values.request,
                &signed_values.signature,
            );
        authorizer.authorize_login(&request)
    };
    let first = open().expect("the first socket");
    assert!(open().is_err());
    drop(first);
    assert!(open().is_ok(), "the slot is free once the socket ends");
}

#[test]
fn refusals_for_one_agent_reach_the_journal_once_a_minute() {
    let signed = signed_in();
    let request = signed.json(ANSWER_PATH, b"{}");
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
        git: None,
        diff: None,
        logins: None,
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
    let values = signed.sign("POST", ANSWER_PATH, b"{}");
    let head = head_of(&signed, ANSWER_PATH, b"{}", &values);
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

#[test]
fn the_pages_style_policy_follows_the_live_switches() {
    let assets = BTreeMap::new();
    let memory = MemoryReads::new();
    let board = TicketLimits::new();
    let peer = outside();
    let switches = SharedSwitches::new(Switches {
        hire: false,
        terminal: false,
        ..all_on()
    });
    let catalog = Catalog {
        assets: &assets,
        manifest: &[],
        switches: &switches,
        git: None,
        diff: None,
        logins: None,
        board: &board,
        memory: &memory,
        peer: &peer,
    };
    assert_eq!(catalog.csp(), CSP);
    switches.store(Switches {
        hire: true,
        terminal: false,
        ..all_on()
    });
    assert_eq!(
        catalog.csp(),
        TERMINAL_CSP,
        "turning hiring on by reload loosens it"
    );
    switches.store(Switches {
        hire: false,
        terminal: true,
        ..all_on()
    });
    assert_eq!(catalog.csp(), TERMINAL_CSP);
    switches.store(Switches {
        hire: false,
        terminal: false,
        ..all_on()
    });
    assert_eq!(catalog.csp(), CSP, "and turning both off tightens it again");
}
