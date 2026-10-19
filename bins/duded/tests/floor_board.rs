//! The Floor's ticket routes, on the real daemon over a stand-in engine: what the switch gates,
//! what each route refuses before any effect, who authors each record, and what a snapshot may
//! carry. No model is called: the process host is the fake one, a turn ends when the test says so
//! and the check runner answers from a script.
#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};
use duded::config::load_operator;
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use duded::runtime::{OnboardingFailurePoint, OnboardingPausePoint};
use duded::settings::{FloorControl, FloorSettings};
use duded::startup::Witness;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_proto::{Actor, AgentBudget, ExitStatus, GoalBudget, Predicate, Record, TurnOutcome};
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;
use dudes_supervisor::Input;
use dudes_types::{AgentId, IsolationMode, TurnId, Usd};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn check(id: &str, program: &str) -> EnrolledCheck {
    EnrolledCheck {
        id: id.to_owned(),
        label: format!("{id} passes"),
        predicate: Predicate::Command {
            argv: vec![program.to_owned()],
            timeout_s: 10,
        },
        may_already_pass: true,
    }
}

fn profile(id: &str, cwd: &Path) -> Profile {
    Profile {
        id: id.to_owned(),
        label: format!("{id} workspace"),
        checks: vec![
            check("default", common::true_program()),
            check("second", common::true_program()),
        ],
        cwd: cwd.to_path_buf(),
        written_cwd: cwd.to_path_buf(),
        department: "engineering".to_owned(),
        isolation: IsolationMode::Default,
        agent_budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        goal_budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 4,
        mark: None,
    }
}

/// Settings that enroll one project, serve the Floor and let it control agents, change goals and,
/// when `board` is true, work tickets.
fn settings(fixture: &Fixture, board: bool) -> (duded::Settings, PathBuf) {
    let repo = fixture.state.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo = repo.canonicalize().unwrap();
    let mut settings = fixture.settings();
    settings.enrollment = Enrollment {
        profiles: vec![profile("service", &repo)],
    };
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: false,
        renew: false,
    };
    settings.floor_board_enabled = board;
    (settings, repo)
}

struct Board {
    running: common::Running,
    host: SharedHost,
    settings: duded::Settings,
    addr: SocketAddr,
    repo: PathBuf,
    pause: duded::runtime::OnboardingPause,
    failure: duded::runtime::OnboardingFailure,
    authorizer: duded::floor_session::FloorSessionAuthorizer,
    operator:
        Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>,
}

impl Board {
    fn boot(
        settings: duded::Settings,
        repo: PathBuf,
        answers: Vec<duded::predicate::PredicateOutcome>,
    ) -> Self {
        Self::boot_with(settings, repo, Arc::new(ScriptedRunner::scripted(answers)))
    }

    fn boot_with<P: duded::predicate::PredicateRunner>(
        settings: duded::Settings,
        repo: PathBuf,
        runner: Arc<P>,
    ) -> Self {
        let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
        let host =
            SharedHost::with_native_evidence(settings.state_dir.join(duded::boot::JOURNAL_DIR));
        let seams = common::seams_with(
            &settings,
            host.clone(),
            runner,
            Arc::new(TestClock::new()),
            Witness::default(),
            column as Arc<dyn dudes_rpc::auth::PeerResolver>,
        );
        host.bind_child_ends(Arc::clone(&seams.children));
        let daemon = duded::boot(&settings, seams).expect("boot");
        let addr = daemon.floor_addr().expect("the Floor listener");
        let pause = daemon.onboarding_pause();
        let failure = daemon.onboarding_failure();
        let authorizer = daemon.floor_session_authorizer().expect("sessions");
        let running = common::start(daemon);
        let operator = Client::connect(settings.socket()).unwrap();
        Self {
            running,
            host,
            settings,
            addr,
            repo,
            pause,
            failure,
            authorizer,
            operator,
        }
    }

    fn hire(&self, name: &str) -> AgentId {
        let params = dudes_proto::rpc::agent::HireParams {
            name: name.to_owned(),
            department: "engineering".to_owned(),
            ..common::hire_params(&self.repo)
        };
        match self
            .running
            .handle
            .ask(duded::ports::Ask::Hire(Box::new(params)))
            .expect("the hire")
        {
            duded::ports::Answered::Hired(result) => result.agent,
            other => panic!("agent.hire answered {other:?}"),
        }
    }

    fn rpc(&mut self, method: Method, params: Value) -> dudes_proto::rpc::Response {
        let credential = OperatorCredential::new(
            std::fs::read_to_string(
                self.settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .unwrap(),
        )
        .unwrap();
        self.operator
            .call_as_operator(method, Some(params), credential)
            .unwrap()
    }

    fn page(&self) -> Page {
        let credential = std::fs::read_to_string(
            self.settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential");
        let mut client = Client::connect(self.settings.socket()).expect("daemon socket");
        let reply = client
            .call_as_operator(
                Method::FloorSessionIssue,
                None,
                OperatorCredential::new(credential).expect("credential"),
            )
            .expect("issue response");
        let issued: SessionIssueResult =
            serde_json::from_value(reply.result.expect("issued")).expect("typed issue");
        let (cookie, csrf) = exchange(self.addr, &issued.bootstrap);
        Page {
            addr: self.addr,
            cookie,
            csrf,
        }
    }

    /// The agent's engine finishes `turn` and says `message`.
    fn end_turn(&self, fixture: &Fixture, turn: TurnId, message: Option<&str>) {
        common::end_turn_with_native_payload(
            fixture,
            &self.host,
            Input::ChildExited {
                final_reply: None,
                elapsed_ms: Some(5),
                turn,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                model_usage: std::collections::BTreeMap::new(),
                outcome: TurnOutcome::Completed,
                last_message: message.map(str::to_owned),
            },
        );
    }

    fn stop(mut self) {
        drop(self.operator);
        self.running.stop(duded::ExitCause::ShutdownRpc);
    }
}

/// One signed-in browser page: the cookie and the CSRF token it holds.
#[derive(Clone)]
struct Page {
    addr: SocketAddr,
    cookie: String,
    csrf: String,
}

struct Answer {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        if self.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&self.body).expect("JSON body")
        }
    }

    #[track_caller]
    fn ok(&self) -> Value {
        assert_eq!(self.status, 200, "{} {:?}", self.head, self.json());
        self.json()
    }

    #[track_caller]
    fn refused(&self, reason: &str) {
        assert_eq!(self.status, 409, "{} {:?}", self.head, self.json());
        assert_eq!(self.json(), json!({"state": "refused", "reason": reason}));
        assert!(self.body.len() <= 512);
    }

    #[track_caller]
    fn empty(&self, status: u16) {
        assert_eq!(self.status, status, "{}", self.head);
        assert!(self.body.is_empty(), "{:?}", self.json());
    }
}

impl Page {
    fn send(&self, request: &[u8]) -> Answer {
        let mut stream = TcpStream::connect(self.addr).expect("Floor listener");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.write_all(request).expect("request");
        let head = read_head(&mut stream);
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status");
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).expect("response body");
        Answer { status, head, body }
    }

    fn raw(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
        let mut request = format!("POST {path} HTTP/1.1\r\n");
        for (name, value) in headers {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        let _ = write!(request, "Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(body);
        self.send(&bytes)
    }

    fn headers(&self) -> Vec<(String, String)> {
        let host = self.addr.to_string();
        vec![
            ("Host".into(), host.clone()),
            ("Origin".into(), format!("http://{host}")),
            (
                "Cookie".into(),
                format!("dudes_floor_session={}", self.cookie),
            ),
            ("X-Dudes-CSRF".into(), self.csrf.clone()),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn post_bytes(&self, path: &str, body: &[u8]) -> Answer {
        let headers = self.headers();
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        self.raw(path, &pairs, body)
    }

    fn post(&self, path: &str, body: &Value) -> Answer {
        self.post_bytes(path, &serde_json::to_vec(body).unwrap())
    }

    fn create(&self, body: &Value) -> Answer {
        self.post("/floor/ticket", body)
    }

    /// The ticket's card, sequence and all, as the read route shows it.
    fn read(&self, card: &str) -> Value {
        self.post("/floor/ticket/read", &json!({"card": card})).ok()
    }

    fn seq(&self, card: &str) -> u64 {
        self.read(card)["card"]["seq"].as_u64().expect("a sequence")
    }

    fn completed_detail(&self, card: &str) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let detail = self.read(card);
            if detail["ticket"]["instruction_review"]["pending_comparison"] == false {
                return detail;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "instruction review is still pending"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn act(&self, verb: &str, card: &str) -> Answer {
        let detail = self.read(card);
        let mut body = json!({"card": card, "expected_seq": detail["card"]["seq"]});
        let review = &detail["ticket"]["instruction_review"];
        if verb == "accept" && review["pending_comparison"] == false {
            body["review_watermark"] = review["watermark"].clone();
        }
        self.post(&format!("/floor/ticket/{verb}"), &body)
    }

    fn give(&self, card: &str, agent: AgentId, message: Option<&str>) -> Answer {
        let mut body = json!({"card": card, "agent": agent, "expected_seq": self.seq(card)});
        if let Some(message) = message {
            body["message"] = json!(message);
        }
        self.post("/floor/ticket/assign", &body)
    }

    fn snapshot(&self) -> (Value, Vec<u8>) {
        snapshot(self.addr, &self.cookie, &self.csrf)
    }
}

#[track_caller]
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("a response head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

fn exchange(addr: SocketAddr, bootstrap: &str) -> (String, String) {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{bootstrap}",
        bootstrap.len()
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

/// The first snapshot frame of a new stream, parsed and as bytes.
fn snapshot(addr: SocketAddr, cookie: &str, csrf: &str) -> (Value, Vec<u8>) {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("upgrade");
    assert!(read_head(&mut stream).starts_with("HTTP/1.1 101 Switching Protocols"));
    let subscribe = br#"{"type":"floor.subscribe"}"#;
    let mut frame = vec![
        0x81,
        0x80 | u8::try_from(subscribe.len()).expect("small frame"),
    ];
    let mask = [1_u8, 2, 3, 4];
    frame.extend_from_slice(&mask);
    frame.extend(
        subscribe
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    stream.write_all(&frame).expect("subscribe");
    let mut head = [0; 2];
    stream.read_exact(&mut head).expect("snapshot frame");
    assert_eq!(head[0], 0x81);
    let length = match head[1] & 0x7f {
        n @ 0..=125 => usize::from(n),
        126 => {
            let mut bytes = [0; 2];
            stream.read_exact(&mut bytes).expect("u16 length");
            usize::from(u16::from_be_bytes(bytes))
        }
        _ => {
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).expect("u64 length");
            usize::try_from(u64::from_be_bytes(bytes)).expect("bounded snapshot")
        }
    };
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).expect("snapshot payload");
    (
        serde_json::from_slice(&payload).expect("snapshot JSON"),
        payload,
    )
}

fn tickets_made(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::TicketCreated { .. }))
        .count()
}

fn id_of(created: &Value) -> String {
    created["card"].as_str().expect("a card id").to_owned()
}

/// A title-only ticket made through the page: 200, the card and sequence, the position the answer
/// is true through, and a body far under its bound.
fn make(page: &Page, title: &str) -> String {
    let answer = page.create(&json!({"title": title}));
    let created = answer.ok();
    assert!(answer.body.len() <= 512);
    id_of(&created)
}

fn digest_of(cookie: &str) -> [u8; 32] {
    Sha256::digest(cookie.as_bytes()).into()
}

// ---- the switch ------------------------------------------------------------------------------

fn config_with(extra: &str) -> Result<duded::config::OperatorConfig, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, extra).unwrap();
    load_operator(&path).map_err(|error| error.to_string())
}

/// `[floor.board]` is off unless written, and needs both `[floor.control]` and `[floor.goal]`.
///
/// Mutant `SWITCH_WITHOUT_GOAL`: loading accepts the board without a goal switch, and the refusal
/// cases below load.
#[test]
fn the_board_switch_is_off_by_default_and_needs_control_and_goal() {
    assert!(!config_with("").unwrap().board_enabled);
    let on = "[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.board]\nenabled = true\n";
    let loaded = config_with(on).unwrap();
    assert!(loaded.board_enabled && loaded.control.goal());
    for (name, text) in [
        ("board alone", "[floor.board]\nenabled = true\n"),
        (
            "board with control only",
            "[floor.control]\nenabled = true\n[floor.board]\nenabled = true\n",
        ),
    ] {
        let error = config_with(text).expect_err(name);
        assert!(error.contains("[floor.board]"), "{name}: {error}");
    }
    config_with("[floor.goal]\nenabled = true\n[floor.board]\nenabled = true\n")
        .expect_err("board with goal only: goal itself needs control");
    assert!(
        !config_with("[floor.control]\nenabled = true\n[floor.board]\nenabled = false\n")
            .unwrap()
            .board_enabled
    );
    assert!(config_with("[floor.board]\nenabled = true\nextra = 1\n").is_err());
}

/// With the switch off, every ticket path answers exactly as a path that was never routed, and
/// the snapshot says nothing of tickets.
///
/// Mutant `ROUTE_WHEN_SWITCH_OFF`: the paths are routed whatever the switch says.
#[test]
fn with_the_switch_off_the_ticket_paths_are_not_routes() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, false);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let unrouted = page.post("/floor/unrouted", &json!({"title": "x"}));
    for path in [
        "/floor/ticket",
        "/floor/ticket/edit",
        "/floor/ticket/assign",
        "/floor/ticket/accept",
        "/floor/ticket/stop",
        "/floor/ticket/remove",
        "/floor/ticket/read",
    ] {
        let answer = page.post(path, &json!({"title": "x"}));
        assert_eq!(answer.status, unrouted.status, "{path}");
        assert_eq!(answer.body, unrouted.body, "{path}");
    }
    assert_eq!(tickets_made(&fixture), 0);
    let (view, _) = page.snapshot();
    assert!(view.get("ticket").is_none(), "no sibling when off");
    // A ticket made from the socket is a card like any other while the board is off.
    let mut board = board;
    board
        .rpc(Method::TicketCreate, json!({"title": "From the socket"}))
        .result
        .expect("made");
    let (view, _) = page.snapshot();
    let cards = view["projection"]["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert!(
        cards[0].get("ticket").is_none(),
        "no ticket member when off"
    );
    board.stop();
}

/// With the switch on the snapshot carries the empty `ticket` sibling and says so in settings.
#[test]
fn with_the_switch_on_the_snapshot_carries_the_ticket_sibling() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let (view, _) = board.page().snapshot();
    assert_eq!(view["ticket"], json!({}));
    board.stop();
}

// ---- create, and who authored it -------------------------------------------------------------

/// A title-only ticket is made by the session, the record names no request identity, and the
/// snapshot's card carries codes and counts and never the note.
///
/// Mutants `DESCRIPTION_IN_SNAPSHOT` (the snapshot's ticket object carries the note) and
/// `BROWSER_CHOOSES_ARGV` (the body may name a command; see the refusal test below).
#[test]
fn a_title_only_ticket_is_authored_by_the_session_and_the_snapshot_holds_no_note() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let answer =
        page.create(&json!({"title": "Fix the flaky test", "description": "NOTE-IN-THE-RECORD"}));
    let created = answer.ok();
    assert!(answer.body.len() <= 512, "{}", answer.body.len());
    assert_eq!(created["state"], "created");
    assert_eq!(created["seq"], 0);
    assert!(created["lsn"].as_str().unwrap().parse::<u64>().unwrap() > 0);
    let card = id_of(&created);

    let records = fixture.records();
    let Some(Record::TicketCreated {
        title,
        description,
        project,
        done_when,
        by,
        ..
    }) = records
        .iter()
        .find(|record| matches!(record, Record::TicketCreated { .. }))
    else {
        panic!("a ticket record");
    };
    assert_eq!(title, "Fix the flaky test");
    assert_eq!(description, "NOTE-IN-THE-RECORD");
    assert_eq!(project, "service");
    assert_eq!(done_when.id, "default");
    let Actor::BrowserSession { session_digest } = by else {
        panic!("authored by {by:?}");
    };
    assert_eq!(session_digest.bytes(), digest_of(&page.cookie));

    let (view, payload) = page.snapshot();
    let cards = view["projection"]["cards"].as_array().expect("cards");
    let shown = cards.iter().find(|row| row["id"] == json!(card)).unwrap();
    assert_eq!(shown["ticket"], json!({}));
    assert!(
        !String::from_utf8_lossy(&payload).contains("NOTE-IN-THE-RECORD"),
        "the snapshot never carries a note"
    );
    board.stop();
}

/// The read route is where the note, the summary and the check result are, and the snapshot
/// stays silent about them.
///
/// Mutant `SUMMARY_IN_SNAPSHOT`: the agent's last words ride in the ticket object.
#[test]
fn the_summary_and_the_check_result_come_only_from_the_read_route() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let agent = board.hire("Cody");
    let page = board.page();
    let created = page
        .create(&json!({"title": "Ship it", "description": "THE-NOTE"}))
        .ok();
    let card = id_of(&created);
    let given = page.give(&card, agent, None).ok();
    assert_eq!(given["start"], json!({"state": "started"}));
    let turn = fixture.await_turn();
    board.end_turn(&fixture, turn, Some("SUMMARY-SENTINEL <b>bold</b>"));
    fixture.await_record("qualified goal completion", |record| {
        matches!(record, Record::LoopDecided { agent: seen, decision: dudes_proto::LoopDecision::Done, .. } if *seen == agent).then_some(())
    });
    let detail = page.completed_detail(&card);
    assert_eq!(detail["ticket"]["note"], "THE-NOTE");
    assert_eq!(detail["ticket"]["summary"], "SUMMARY-SENTINEL <b>bold</b>");
    assert_eq!(detail["ticket"]["check_result"]["pass"], true);
    assert_eq!(detail["ticket"]["round"], 1);
    assert_eq!(detail["card"]["status"], "blocked");
    assert_eq!(detail["ticket"]["reason"], "shared_instructions_unknown");
    assert!(detail.to_string().len() <= 24_576);

    let (view, payload) = page.snapshot();
    let text = String::from_utf8_lossy(&payload);
    assert!(
        !text.contains("SUMMARY-SENTINEL"),
        "no summary in a snapshot"
    );
    let shown = view["projection"]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(card))
        .unwrap();
    assert!(
        !shown.to_string().contains("THE-NOTE"),
        "the card carries no note (the agent's own goal text is its goal's, not the card's)"
    );
    assert_eq!(shown["status"], "blocked");
    assert_eq!(shown["ticket"]["round"], 1);
    board.stop();
}

///
/// Mutant `FLOOR_ACCEPT_AS_PEER`: the daemon records the accept as the operator's socket.
#[test]
fn a_floor_accept_is_authored_by_the_session_and_starts_the_next_ticket() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let agent = board.hire("Cody");
    let page = board.page();
    let first = make(&page, "First");
    let second = make(&page, "Second");
    page.give(&first, agent, None).ok();
    let queued = page.give(&second, agent, None).ok();
    assert_eq!(queued["start"]["state"], "queued");
    assert_eq!(queued["start"]["position"], 1);
    let turn = fixture.await_turn();
    board.end_turn(&fixture, turn, Some("done"));
    fixture.await_record("qualified goal completion", |record| {
        matches!(record, Record::LoopDecided { agent: seen, decision: dudes_proto::LoopDecision::Done, .. } if *seen == agent).then_some(())
    });
    page.completed_detail(&first);
    let accepted = page.act("accept", &first).ok();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted["seq"], page.seq(&first));

    let accepts: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::CardTransition {
                to: dudes_types::CardStatus::Done,
                by,
                ..
            }
            | Record::TicketInstructionReviewed {
                by,
                action:
                    dudes_proto::InstructionReviewAction::Accept {
                        completion: dudes_proto::InstructionAcceptCompletion::Completed { .. },
                    },
                ..
            } => Some(by),
            _ => None,
        })
        .collect();
    assert_eq!(accepts.len(), 1);
    let Actor::BrowserSession { session_digest } = accepts[0] else {
        panic!("the accept was authored by {:?}", accepts[0]);
    };
    assert_eq!(session_digest.bytes(), digest_of(&page.cookie));
    fixture.await_record("the second ticket started", |record| match record {
        Record::TicketStarted { card, .. } if card.to_string() == second => Some(()),
        _ => None,
    });
    board.stop();
}

// ---- retries, refusals and bounds ------------------------------------------------------------

/// Every action retries by `expected_seq`: after success the page that retries finds a newer
/// sequence and is told so, and nothing is appended.
#[test]
fn a_retry_after_success_is_a_stale_card() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let agent = board.hire("Cody");
    let page = board.page();
    let card = make(&page, "Edit then give");
    let seq = page.seq(&card);
    let edit = json!({"card": card, "expected_seq": seq, "title": "Renamed"});
    page.post("/floor/ticket/edit", &edit).ok();
    page.post("/floor/ticket/edit", &edit).refused("stale_card");

    let give = json!({"card": card, "agent": agent, "expected_seq": page.seq(&card)});
    page.post("/floor/ticket/assign", &give).ok();
    page.post("/floor/ticket/assign", &give)
        .refused("stale_card");

    let turn = fixture.await_turn();
    let stop = json!({"card": card, "expected_seq": page.seq(&card)});
    page.post("/floor/ticket/stop", &stop).ok();
    page.post("/floor/ticket/stop", &stop).refused("stale_card");

    board.end_turn(&fixture, turn, None);
    page.completed_detail(&card);
    let acknowledged = page.act("accept", &card).ok();
    assert_eq!(acknowledged["outcome"], "instructions_acknowledged");
    assert_eq!(page.read(&card)["card"]["status"], "todo");
    let remove = json!({"card": card, "expected_seq": page.seq(&card)});
    page.post("/floor/ticket/remove", &remove).ok();
    page.post("/floor/ticket/remove", &remove)
        .refused("stale_card");
    board.stop();
}

/// Each refusal before an append is `409` with a fixed code and a body of a few bytes, and appends
/// nothing.
#[test]
fn refusals_are_fixed_codes_and_append_nothing() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let mut board = Board::boot(settings, repo, vec![common::passes()]);
    let agent = board.hire("Cody");
    let page = board.page();
    let card = make(&page, "Fresh");
    let before = fixture.records();

    page.act("accept", &card).refused("wrong_state");
    page.act("stop", &card).refused("wrong_state");
    page.create(&json!({"title": "x", "project": "no_such_project"}))
        .refused("project_unavailable");
    page.create(
        &json!({"title": "x", "done_when": "default", "done_when_revision": "0".repeat(64)}),
    )
    .refused("done_when_changed");
    let stranger = AgentId::from_parts(9, 9);
    page.give(&card, stranger, None)
        .refused("agent_unavailable");
    page.post(
        "/floor/ticket/accept",
        &json!({"card": card, "expected_seq": 99}),
    )
    .refused("stale_card");
    let missing = dudes_types::CardId::from_parts(9, 9);
    page.post(
        "/floor/ticket/accept",
        &json!({"card": missing, "expected_seq": 0}),
    )
    .refused("stale_card");

    let plain = board.rpc(
        Method::BoardCreate,
        json!({"title": "A plain card", "department": "engineering"}),
    );
    let plain = plain.result.expect("a plain card")["card"]
        .as_str()
        .expect("id")
        .to_owned();
    page.post(
        "/floor/ticket/accept",
        &json!({"card": plain, "expected_seq": 0}),
    )
    .refused("not_a_ticket");
    assert_eq!(
        fixture
            .records()
            .into_iter()
            .filter(|record| !matches!(record, Record::CardCreated { .. }))
            .count(),
        before.len(),
        "a refusal appends nothing"
    );

    // Eight tickets wait for an agent whose project is held by a ninth.
    let running = make(&page, "Running");
    page.give(&running, agent, None).ok();
    let mut queued = Vec::new();
    for n in 0..8 {
        let queued_card = make(&page, &format!("Queued {n}"));
        page.give(&queued_card, agent, None).ok();
        queued.push(queued_card);
    }
    let ninth = make(&page, "One too many");
    page.give(&ninth, agent, None).refused("queue_full");
    board.stop();
}

/// Bodies are one flat object of the route's members: an unknown member (an author, a program),
/// a duplicate, a nested value, an oversized body or a query is refused before any effect.
///
/// Mutant `BROWSER_CHOOSES_ARGV`: a body member may name the command a check runs.
#[test]
fn bodies_are_bounded_flat_and_name_no_author_or_command() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let before = fixture.records();
    for body in [
        r#"{"title":"x","by":{"kind":"system"}}"#,
        r#"{"title":"x","argv":["rm","-rf","/"]}"#,
        r#"{"title":"x","done_when":"default"}"#,
        r#"{"title":"mirror \u202egnp.exe"}"#,
        r#"{"title":"x","predicate":{"kind":"command","argv":["x"]}}"#,
        r#"{"title":"x","title":"y"}"#,
        r#"{"title":{"a":{"b":{"c":{"d":{"e":{"f":{"g":{"h":{"i":1}}}}}}}}}}"#,
        r#"["x"]"#,
        r#"{"title":"x""#,
        "",
    ] {
        page.post_bytes("/floor/ticket", body.as_bytes()).empty(400);
    }
    let big = format!(r#"{{"title":"{}"}}"#, "x".repeat(24_577));
    page.post_bytes("/floor/ticket", big.as_bytes()).empty(413);
    let at_the_cap = format!(r#"{{"title":"x","description":"{}"}}"#, "y".repeat(8_300));
    page.post_bytes("/floor/ticket", at_the_cap.as_bytes())
        .empty(400);
    let queried = page.post_bytes("/floor/ticket?as=system", br#"{"title":"x"}"#);
    assert_ne!(queried.status, 200);
    assert_eq!(fixture.records(), before);
    board.stop();
}

/// The transport rules every action shares: exact Host and Origin, a live cookie and the current
/// CSRF token. An agent's token or a bearer header alone authenticates nothing. All answer the
/// uniform empty `403` and change nothing.
#[test]
fn transport_rules_refuse_with_the_uniform_forbidden() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let before = fixture.records();
    let body = br#"{"title":"x"}"#;
    let host = page.addr.to_string();
    let cookie = format!("dudes_floor_session={}", page.cookie);
    let origin = format!("http://{host}");
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "wrong origin",
            vec![
                ("Host", host.as_str()),
                ("Origin", "http://evil.example"),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "no origin",
            vec![
                ("Host", host.as_str()),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "wrong host",
            vec![
                ("Host", "evil.example"),
                ("Origin", origin.as_str()),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "stale csrf",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("Cookie", cookie.as_str()),
                (
                    "X-Dudes-CSRF",
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                ),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "no cookie",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "a bearer token alone",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("Authorization", "Bearer 0123456789abcdef0123456789abcdef"),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "an agent token in the cookie slot",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                (
                    "Cookie",
                    "dudes_floor_session=0123456789abcdef0123456789abcdef",
                ),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
    ];
    for (name, headers) in cases {
        let answer = page.raw("/floor/ticket", &headers, body);
        assert_eq!(answer.status, 403, "{name}: {}", answer.head);
        assert!(answer.body.is_empty(), "{name}");
    }
    assert_eq!(fixture.records(), before);
    board.stop();
}

// ---- limits, the permit and a lost answer ----------------------------------------------------

/// A session creates at most 64 tickets and assigns at most 32 times; the next is refused
/// `limit_reached` with nothing appended, and another session starts its own counts.
#[test]
fn a_session_has_64_creates_and_32_assigns() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let agent = board.hire("Cody");
    let other = board.hire("Dana");
    let page = board.page();
    for n in 0..64 {
        let answer = page.create(&json!({"title": format!("Ticket {n}")}));
        assert_eq!(
            answer.status,
            200,
            "{n}: {}",
            String::from_utf8_lossy(&answer.body)
        );
    }
    page.create(&json!({"title": "The 65th"}))
        .refused("limit_reached");
    assert_eq!(tickets_made(&fixture), 64);
    let second = board.page();
    make(&second, "A new session starts at zero");

    let cards: Vec<String> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::TicketCreated { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    let mut accepted = 0;
    // The first assignment starts a ticket and holds the project, so the rest queue.
    page.give(&cards[0], agent, None).ok();
    accepted += 1;
    let movable = &cards[1];
    let mut refused = None;
    for n in 0..40 {
        let target = if n % 2 == 0 { agent } else { other };
        let answer = page.give(movable, target, None);
        if answer.status == 200 {
            accepted += 1;
        } else {
            refused = Some(answer);
            break;
        }
    }
    let refused = refused.expect("the assignments run out");
    refused.refused("limit_reached");
    assert_eq!(accepted, 32, "the 33rd assignment is the one refused");
    board.stop();
}

/// One request at a time per session: a second request while the first waits for the writer is
/// refused `busy`, and the first still completes.
#[test]
fn a_session_has_one_request_in_flight() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    board.pause.arm(OnboardingPausePoint::BeforeTicketPermit);
    let first = {
        let page = page.clone();
        std::thread::spawn(move || page.create(&json!({"title": "First"})))
    };
    assert!(board.pause.wait_reached(Duration::from_secs(5)));
    let (tx, rx) = std::sync::mpsc::channel();
    let second = {
        let page = page.clone();
        std::thread::spawn(move || {
            let _ = tx.send(page.create(&json!({"title": "Second"})));
        })
    };
    let answered = rx.recv_timeout(Duration::from_secs(3));
    board.pause.release();
    second.join().unwrap();
    answered
        .expect("a second request is answered without waiting for the writer")
        .refused("busy");
    first.join().unwrap().ok();
    assert_eq!(tickets_made(&fixture), 1);
    board.stop();
}

/// A session revoked between the checks and the append permit wins nothing: the uniform `403`, and
/// the journal is as it was.
#[test]
fn a_session_revoked_at_the_permit_gets_the_uniform_forbidden() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let before = fixture.records();
    board.pause.arm(OnboardingPausePoint::BeforeTicketPermit);
    let loser = {
        let page = page.clone();
        std::thread::spawn(move || page.create(&json!({"title": "Never"})))
    };
    assert!(board.pause.wait_reached(Duration::from_secs(5)));
    assert!(board.authorizer.revoke_for_test(&page.cookie));
    board.pause.release();
    loser.join().unwrap().empty(403);
    assert_eq!(fixture.records(), before);
    board.stop();
}

/// An answer lost after the record was fsynced leaves one ticket and a `503`. The page never
/// retries a create by itself, so the journal holds one.
#[test]
fn a_create_lost_after_the_fsync_leaves_exactly_one_ticket() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    board.failure.arm(OnboardingFailurePoint::Ticket);
    page.create(&json!({"title": "Maybe saved"})).empty(503);
    assert_eq!(tickets_made(&fixture), 1);
    board.stop();
}

// ---- stop, remove and the old decoder --------------------------------------------------------

/// Stop returns a Working ticket to To do and Remove takes it off the board, both through the
/// page, and the snapshot follows.
#[test]
fn stop_and_remove_take_a_ticket_off_the_agent_and_the_board() {
    let fixture = Fixture::new();
    let (settings, repo) = settings(&fixture, true);
    let board = Board::boot(settings, repo, vec![common::fails()]);
    let agent = board.hire("Cody");
    let page = board.page();
    let card = make(&page, "Stop me");
    page.give(&card, agent, None).ok();
    let turn = fixture.await_turn();
    page.act("remove", &card)
        .refused("instruction_comparison_pending");
    let stopped = page.act("stop", &card).ok();
    assert_eq!(stopped["state"], "stopped");
    let detail = page.read(&card);
    assert_eq!(detail["card"]["status"], "todo");
    assert_eq!(detail["card"]["claimed_by"], Value::Null);
    let (view, _) = page.snapshot();
    let shown = view["projection"]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(card))
        .unwrap()
        .clone();
    assert_eq!(shown["ticket"]["stopping"], true, "the turn is still open");
    board.end_turn(&fixture, turn, None);
    page.completed_detail(&card);
    let acknowledged = page.act("accept", &card).ok();
    assert_eq!(acknowledged["outcome"], "instructions_acknowledged");
    assert_eq!(page.read(&card)["card"]["status"], "todo");
    let removed = page.act("remove", &card).ok();
    assert_eq!(removed["state"], "removed");
    assert!(removed.get("seq").is_none());
    let (view, _) = page.snapshot();
    assert!(
        !view["projection"]["cards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == json!(card))
    );
    board.stop();
}

/// The daemon sends codes, and the page words them from its own table. Each code is pinned to its
/// sentence in that table, character for character, so the two cannot drift apart.
#[test]
fn the_page_words_match_the_daemon_words() {
    use dudes_proto::{TicketReason, TicketRefusal, TicketWait};
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../floor/src/renderer/ticket/words.ts");
    let source = std::fs::read_to_string(&path).expect("the page's table of ticket words");
    let pinned = |key: &str, sentence: &str| {
        assert!(
            source.contains(&format!("{key}: \"{sentence}\",")),
            "{key} is not worded {sentence:?}"
        );
    };
    for reason in [
        TicketReason::ProjectUnavailable,
        TicketReason::CheckCannotRun,
        TicketReason::CeilingRequired,
        TicketReason::ReserveMissing,
        TicketReason::GoalChanged,
        TicketReason::LimitGoal,
        TicketReason::Slowed,
        TicketReason::NoTurns,
        TicketReason::HeldLaunch,
        TicketReason::HeldFailing,
        TicketReason::HeldUnsure,
        TicketReason::WorktreeMissing,
    ] {
        let key = serde_json::to_value(reason).unwrap();
        pinned(key.as_str().unwrap(), reason.sentence());
    }
    for refusal in [
        TicketRefusal::NotATicket,
        TicketRefusal::TicketCard,
        TicketRefusal::ProjectUnavailable,
        TicketRefusal::DoneWhenChanged,
        TicketRefusal::AgentUnavailable,
        TicketRefusal::AgentNoProject,
        TicketRefusal::QueueFull,
        TicketRefusal::LimitReached,
        TicketRefusal::Busy,
    ] {
        pinned(
            refusal.code(),
            &refusal.sentence(dudes_types::CardStatus::Todo),
        );
    }
    // The page has already fetched the latest when it says this, so its sentence differs from
    // the one a command line prints.
    assert!(source.contains("stale_card: \"This ticket changed while you were looking. "));
    assert!(source.contains("`You can't do that to a ticket that is ${"));
    assert_eq!(
        TicketRefusal::WrongState.sentence(dudes_types::CardStatus::Review),
        "You can't do that to a ticket that is Ready for you."
    );
    for (status, name) in [
        ("todo", "To do"),
        ("claimed", "To do"),
        ("doing", "Working"),
        ("review", "Ready for you"),
        ("done", "Done"),
        ("blocked", "Needs you"),
    ] {
        assert!(
            source.contains(&format!("case '{status}': return '{name}';"))
                || source.contains(&format!("case '{status}':\n    case ")),
            "{status} is not named {name}"
        );
    }
    for (wait, kind) in [
        (TicketWait::NotGiven, "not_given"),
        (TicketWait::Closing, "closing"),
        (TicketWait::ProjectBusy, "project_busy"),
        (TicketWait::Stopping, "stopping"),
        (TicketWait::AgentLimit, "agent_limit"),
        (TicketWait::FloorLimit, "floor_limit"),
    ] {
        let sentence = wait.sentence("", "");
        assert!(
            source.contains(&format!("case '{kind}': return \"{sentence}\";")),
            "{kind}"
        );
    }
    let card = dudes_types::CardId::from_parts(1, 1);
    for (wait, kind) in [
        (TicketWait::AgentBusy, "agent_busy"),
        (TicketWait::Ticket { card }, "ticket"),
        (TicketWait::AgentPaused, "agent_paused"),
        (TicketWait::AgentAttached, "agent_attached"),
    ] {
        let sentence = wait.sentence("${agent}", "${title}");
        assert!(
            source.contains(&format!("case '{kind}': return `{sentence}`;")),
            "{kind}"
        );
    }
    let queued = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../floor/src/renderer/ticket/ticket.ts"),
    )
    .expect("the page's ticket module");
    assert!(queued.contains("`Queued for ${agent}, ${placeInLine(start.position)}. "));
    assert!(source.contains("if (position <= 1) return 'next in line';"));
}

// ---- the ticket diff route -------------------------------------------------------------------

/// The check answers from a script; the captured reads the diff route and the ticket captures
/// make run for real.
struct Hybrid {
    scripted: ScriptedRunner,
    real: dudes_host::OneShot,
}

impl duded::predicate::PredicateRunner for Hybrid {
    fn run(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        self.scripted.run(spec, timeout)
    }

    fn run_captured(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
        cap: usize,
    ) -> std::io::Result<duded::predicate::PredicateCaptured> {
        use duded::predicate::PredicateCaptured;
        use dudes_host::CapturedOutcome;
        self.real
            .run_captured(spec, timeout, cap)
            .map(|outcome| match outcome {
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }

    fn sweep(&self) {
        self.scripted.sweep();
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
    let path = std::env::var_os("PATH").expect("PATH");
    let program = std::env::split_paths(&path)
        .map(|bin| bin.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let mut command = vec![program.into_os_string()];
    command.extend(args.iter().map(std::ffi::OsString::from));
    let spec = LaunchSpec {
        argv: command,
        env: vec![
            EnvVar::new("PATH", path),
            EnvVar::new("HOME", repo),
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            EnvVar::new("GIT_CONFIG_COUNT", "1"),
            EnvVar::new("GIT_CONFIG_KEY_0", "maintenance.auto"),
            EnvVar::new("GIT_CONFIG_VALUE_0", "false"),
            EnvVar::new("GIT_AUTHOR_NAME", "Lab"),
            EnvVar::new("GIT_AUTHOR_EMAIL", "lab@example.invalid"),
            EnvVar::new("GIT_COMMITTER_NAME", "Lab"),
            EnvVar::new("GIT_COMMITTER_EMAIL", "lab@example.invalid"),
        ],
        cwd: repo.to_path_buf(),
        stdin: LaunchInput::Null,
    };
    match dudes_host::OneShot::new()
        .run(&spec, Duration::from_secs(60))
        .expect("git runs")
    {
        dudes_host::OneShotOutcome::Finished { exit: Some(0), .. } => {}
        other => panic!("git {args:?}: {other:?}"),
    }
}

/// Settings over a real repository holding `a.txt`, with the Git tab and the diff route switched
/// as asked.
fn diff_settings(fixture: &Fixture, git: bool, diff: bool) -> (duded::Settings, PathBuf) {
    let (mut settings, _) = settings(fixture, true);
    let repo = fixture.state.path().canonicalize().unwrap().join("real");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "old a\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
    settings.enrollment.profiles[0].cwd.clone_from(&repo);
    settings.enrollment.profiles[0]
        .written_cwd
        .clone_from(&repo);
    settings.floor_git_enabled = git;
    settings.floor_diff_enabled = diff;
    settings.predicate_path = std::env::var_os("PATH");
    (settings, repo)
}

fn hybrid(answers: Vec<duded::predicate::PredicateOutcome>) -> Arc<Hybrid> {
    Arc::new(Hybrid {
        scripted: ScriptedRunner::scripted(answers),
        real: dudes_host::OneShot::new(),
    })
}

fn manifest_body(card: &str, seq: u64) -> Value {
    json!({"mode": "manifest", "card": card, "expected_seq": seq})
}

fn refusal_body(reason: &str) -> Value {
    json!({"mode": "refused", "reason": reason})
}

/// `[floor.diff]` is off unless written, has one key, and does not need the Git tab to load
/// (the route needs both at run time).
#[test]
fn the_diff_switch_is_off_by_default_and_closed() {
    assert!(!config_with("").unwrap().diff_enabled);
    assert!(
        !config_with("[floor.diff]\nenabled = false\n")
            .unwrap()
            .diff_enabled
    );
    let on = config_with("[floor.diff]\nenabled = true\n").unwrap();
    assert!(on.diff_enabled && !on.git_enabled);
    let both = config_with("[floor.git]\nenabled = true\n[floor.diff]\nenabled = true\n").unwrap();
    assert!(both.diff_enabled && both.git_enabled);
    assert!(config_with("[floor.diff]\nenabled = true\nextra = 1\n").is_err());
}

/// The route exists only with both switches on. With either off, a POST to it answers exactly as
/// a path that was never routed, and a snapshot advertises nothing.
#[test]
fn the_diff_route_needs_both_the_git_and_the_diff_switch() {
    for (git, diff) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        let (settings, repo) = diff_settings(&fixture, git, diff);
        let board = Board::boot(settings, repo, vec![common::passes()]);
        let page = board.page();
        let unrouted = page.post("/floor/unrouted", &manifest_body("none", 0));
        let answer = page.post(
            "/floor/diff",
            &manifest_body("01J00000000000000000000000", 0),
        );
        if git && diff {
            assert_eq!(answer.status, 200, "{}", answer.head);
            assert_eq!(answer.json(), refusal_body("stale_ticket"));
            assert!(
                answer.head.contains("Cache-Control: no-store"),
                "{}",
                answer.head
            );
            assert!(
                answer.head.contains("Content-Type: application/json"),
                "{}",
                answer.head
            );
        } else {
            assert_eq!(
                (answer.status, &answer.body),
                (unrouted.status, &unrouted.body),
                "git={git} diff={diff}"
            );
            assert!(!answer.head.contains("application/json"), "{}", answer.head);
        }
        let (view, _) = page.snapshot();
        assert_eq!(
            view.get("diff").cloned(),
            (git && diff).then(|| json!({})),
            "git={git} diff={diff}: the sibling is an empty object exactly when the route exists"
        );
        board.stop();
    }
}

/// Every transport rule refuses with the uniform `403` and an empty body before the route reads
/// anything or touches the journal.
#[test]
fn the_diff_route_refuses_transport_failures_with_the_uniform_forbidden() {
    let fixture = Fixture::new();
    let (settings, repo) = diff_settings(&fixture, true, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let before = fixture.records();
    let body = serde_json::to_vec(&manifest_body("01J00000000000000000000000", 0)).unwrap();
    let host = page.addr.to_string();
    let cookie = format!("dudes_floor_session={}", page.cookie);
    let origin = format!("http://{host}");
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "wrong origin",
            vec![
                ("Host", host.as_str()),
                ("Origin", "http://evil.example"),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "no origin",
            vec![
                ("Host", host.as_str()),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "wrong host",
            vec![
                ("Host", "evil.example"),
                ("Origin", origin.as_str()),
                ("Cookie", cookie.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "bad csrf",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("Cookie", cookie.as_str()),
                (
                    "X-Dudes-CSRF",
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                ),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "no csrf",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("Cookie", cookie.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "no cookie",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
        (
            "an unknown session",
            vec![
                ("Host", host.as_str()),
                ("Origin", origin.as_str()),
                (
                    "Cookie",
                    "dudes_floor_session=0123456789abcdef0123456789abcdef",
                ),
                ("X-Dudes-CSRF", page.csrf.as_str()),
                ("Content-Type", "application/json"),
            ],
        ),
    ];
    for (name, headers) in cases {
        let answer = page.raw("/floor/diff", &headers, &body);
        assert_eq!(answer.status, 403, "{name}: {}", answer.head);
        assert!(answer.body.is_empty(), "{name}");
    }
    assert_eq!(fixture.records(), before);
    let ok = page.post(
        "/floor/diff",
        &manifest_body("01J00000000000000000000000", 0),
    );
    assert_eq!(
        ok.status, 200,
        "none of the refusals took the session's turn: {}",
        ok.head
    );
    board.stop();
}

/// A body that is not one flat closed object, or that is over a kilobyte, is a plain `400` or
/// `413` and does not start the session's interval.
#[test]
fn the_diff_route_refuses_malformed_bodies_before_reading_anything() {
    let fixture = Fixture::new();
    let (settings, repo) = diff_settings(&fixture, true, true);
    let board = Board::boot(settings, repo, vec![common::passes()]);
    let page = board.page();
    let id = "a".repeat(32);
    for (body, status) in [
        ("{}".to_owned(), 400),
        ("[]".to_owned(), 400),
        (
            json!({"mode": "manifest", "card": "01J00000000000000000000000", "expected_seq": 0, "path": "/etc/passwd"})
                .to_string(),
            400,
        ),
        (
            json!({"mode": "manifest", "card": "01J00000000000000000000000", "expected_seq": 0, "revision": "HEAD~1"})
                .to_string(),
            400,
        ),
        (
            json!({"mode": "content", "manifest_id": id, "entries": []}).to_string(),
            400,
        ),
        (" ".repeat(1025), 413),
    ] {
        let answer = page.post_bytes("/floor/diff", body.as_bytes());
        assert_eq!(answer.status, status, "{body:?}: {}", answer.head);
    }
    let ok = page.post(
        "/floor/diff",
        &manifest_body("01J00000000000000000000000", 0),
    );
    assert_eq!(
        ok.json(),
        refusal_body("stale_ticket"),
        "no refusal took the interval"
    );
    board.stop();
}

/// A ticket on a real checkout, over HTTP: the manifest names what the agent changed, the stamp
/// the daemon recorded after the check decides the freshness words, a content request returns the
/// files whole, a later edit is named, and a stop makes the old ids stale.
#[test]
fn a_ticket_diff_is_served_from_the_recorded_baseline_and_stamp() {
    let fixture = Fixture::new();
    let (settings, repo) = diff_settings(&fixture, true, true);
    let board = Board::boot_with(settings, repo.clone(), hybrid(vec![common::fails()]));
    let agent = board.hire("Cody");
    let setup = board.page();
    let card = make(&setup, "Fix the parser");
    setup.give(&card, agent, None).ok();
    let first = fixture.await_turn();

    std::fs::write(repo.join("a.txt"), "agent edit\n").unwrap();
    std::fs::write(repo.join("scratch.log"), "untracked and never listed\n").unwrap();
    board.end_turn(&fixture, first, Some("tried"));
    let check_lsn = fixture.await_record("the stamp", |record| match record {
        Record::TicketReviewStamped { stamp, .. } => Some(stamp.check_lsn),
        _ => None,
    });
    let goal = fixture.await_record("the start", |record| match record {
        Record::TicketStarted { goal, .. } => Some(goal.id),
        _ => None,
    });

    let reader = board.page();
    let seq = reader.seq(&card);
    let answer = reader.post("/floor/diff", &manifest_body(&card, seq));
    assert_eq!(answer.status, 200, "{}", answer.head);
    assert!(
        answer.head.contains("Cache-Control: no-store"),
        "{}",
        answer.head
    );
    let wire = String::from_utf8_lossy(&answer.body).into_owned();
    assert!(
        !wire.contains(repo.to_str().unwrap())
            && !wire.contains("scratch.log")
            && !wire.contains("lab@example"),
        "no directory, untracked name or identity in {wire}"
    );
    let page = answer.json();
    assert_eq!(page["mode"], "manifest");
    assert_eq!(page["freshness"], "matches_review_sample");
    assert_eq!(page["untracked"], 1);
    assert_eq!(page["binding"]["card"], card);
    assert_eq!(page["binding"]["seq"], seq);
    assert_eq!(page["binding"]["round"], 1);
    assert_eq!(page["binding"]["goal"], goal.to_string());
    assert_eq!(page["binding"]["claimant"], agent.to_string());
    assert_eq!(page["binding"]["check_lsn"], check_lsn.0.to_string());
    assert_eq!(page["entries"][0]["path"], "a.txt");
    assert_eq!(page["entries"][0]["kind"], "modified");
    let entry = page["entries"][0]["entry_id"].as_str().unwrap().to_owned();
    let manifest_id = page["manifest_id"].as_str().unwrap().to_owned();
    let content_request =
        json!({"mode": "content", "manifest_id": manifest_id, "entries": [entry]});

    let early = reader.post("/floor/diff", &content_request);
    assert_eq!(
        early.json(),
        refusal_body("busy"),
        "the interval has not passed"
    );
    std::thread::sleep(Duration::from_millis(2100));
    let body = reader.post("/floor/diff", &content_request).ok();
    assert_eq!(body["files"][0]["old"], "old a\n");
    assert_eq!(body["files"][0]["current"], "agent edit\n");
    assert_eq!(body["freshness"], "unknown");

    std::fs::write(repo.join("a.txt"), "edited after the check\n").unwrap();
    let later = board.page();
    let changed = later.post("/floor/diff", &manifest_body(&card, seq)).ok();
    assert_eq!(changed["freshness"], "changed_after_check");
    assert_eq!(changed["binding"]["check_lsn"], check_lsn.0.to_string());

    std::thread::sleep(Duration::from_millis(2100));
    let stop = reader.act("stop", &card);
    assert_eq!(stop.status, 200, "{}", stop.head);
    std::thread::sleep(Duration::from_millis(2100));
    let stale = reader.post("/floor/diff", &content_request);
    assert_eq!(
        stale.json(),
        refusal_body("stale_ticket"),
        "a stop ends the claim"
    );
    board.stop();
}

/// A ticket the daemon has no baseline for has no diff.
#[test]
fn a_ticket_that_never_started_has_no_baseline_to_diff() {
    let fixture = Fixture::new();
    let (settings, repo) = diff_settings(&fixture, true, true);
    let board = Board::boot_with(settings, repo, hybrid(vec![common::passes()]));
    let page = board.page();
    let card = make(&page, "Not started");
    let seq = page.seq(&card);
    let answer = page.post("/floor/diff", &manifest_body(&card, seq));
    assert_eq!(answer.json(), refusal_body("baseline_unavailable"));
    board.stop();
}
