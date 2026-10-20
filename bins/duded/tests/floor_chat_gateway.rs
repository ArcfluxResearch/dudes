//! Authenticated browser-note gateway tests.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duded::config::load_operator_with_options;
use duded::ports::Ask;
use duded::settings::{FloorSettings, RunOptions};
use dudes_proto::Record;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_rpc::Client;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Level, Metadata, Subscriber};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, seams};

const REQUEST_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const CANCEL_ID: &str = "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

struct Browser {
    addr: SocketAddr,
    cookie: String,
    csrf: String,
    clock: Arc<TestClock>,
    failure: duded::runtime::AppendFailure,
    host: Option<SharedHost>,
}

fn start(fixture: &Fixture) -> (duded::Settings, common::Running, Browser) {
    start_with_adapters(fixture, None)
}

fn start_with_adapters(
    fixture: &Fixture,
    adapters: Option<dudes_providers::registry::Adapters>,
) -> (duded::Settings, common::Running, Browser) {
    start_with_grants(fixture, false, adapters, None)
}

fn start_with_grants(
    fixture: &Fixture,
    answers: bool,
    adapters: Option<dudes_providers::registry::Adapters>,
    resolver: Option<Arc<dyn dudes_rpc::auth::PeerResolver>>,
) -> (duded::Settings, common::Running, Browser) {
    let mut settings = fixture.settings();
    settings.floor_answer_enabled = answers;
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    start_configured(RunOptions::default(), adapters, settings, resolver)
}

fn start_configured(
    options: RunOptions,
    adapters: Option<dudes_providers::registry::Adapters>,
    settings: duded::Settings,
    resolver: Option<Arc<dyn dudes_rpc::auth::PeerResolver>>,
) -> (duded::Settings, common::Running, Browser) {
    let clock = Arc::new(TestClock::new());
    let host = SharedHost::with_native_evidence(settings.state_dir.join(duded::boot::JOURNAL_DIR));
    let mut seams = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::scripted([])),
        Arc::clone(&clock),
        duded::startup::Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    if let Some(adapters) = adapters {
        seams.adapters = adapters;
    }
    if let Some(resolver) = resolver {
        seams.resolver = duded::classify::Resolver::fixed(resolver);
    }
    let daemon =
        duded::boot_with_options(&settings, seams, options).expect("boot browser-note fixture");
    let (settings, running, mut browser) = connect_browser(settings, daemon, clock);
    browser.host = Some(host);
    (settings, running, browser)
}

fn connect_browser<H, P, C, R>(
    settings: duded::Settings,
    daemon: duded::Daemon<H, P, C, R>,
    clock: Arc<TestClock>,
) -> (duded::Settings, common::Running, Browser)
where
    H: dudes_supervisor::ProcessHost + Send + 'static,
    P: duded::predicate::PredicateRunner,
    C: dudes_supervisor::Clock + Send + Sync + 'static,
    R: dudes_supervisor::Rng + Send + 'static,
{
    let failure = daemon.append_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let running = common::start(daemon);
    let fixture_credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("owned fixture credential");
    let mut client = Client::connect(settings.socket()).expect("fixture operator socket");
    let result = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(fixture_credential).expect("fixture credential type"),
        )
        .expect("session issue");
    let issue: SessionIssueResult =
        serde_json::from_value(result.result.expect("session issue result"))
            .expect("typed session");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{}",
        issue.bootstrap.len(),
        issue.bootstrap
    );
    let (code, head, body) = http(addr, request.as_bytes());
    assert_eq!(code, 200);
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("fixture cookie")
        .to_owned();
    let body: Value = serde_json::from_slice(&body).expect("session exchange JSON");
    let csrf = body["csrf"].as_str().expect("fixture CSRF").to_owned();
    (
        settings,
        running,
        Browser {
            addr,
            cookie,
            csrf,
            clock,
            failure,
            host: None,
        },
    )
}

fn request_id(number: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut wire = [b'A'; 43];
    wire[0] = ALPHABET[number % 64];
    wire[1] = ALPHABET[number / 64];
    String::from_utf8(wire.to_vec()).unwrap()
}

fn paused_goal(fixture: &Fixture, running: &common::Running) -> dudes_types::AgentId {
    let agent = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Pause(agent))
        .expect("pause before goal");
    common::set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    agent
}

fn http(addr: SocketAddr, request: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("Floor connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read bound");
    stream.write_all(request).expect("request write");
    response(&mut stream)
}

fn response(stream: &mut TcpStream) -> (u16, String, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response header");
        head.push(byte[0]);
        assert!(head.len() <= 16_384, "bounded response header");
    }
    let head = String::from_utf8(head).expect("response header encoding");
    let code = head
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("numeric status");
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .map_or(0, |value| value.parse().expect("response length"));
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("response body");
    (code, head, body)
}

impl Browser {
    fn head(&self, route: &str, length: usize) -> String {
        format!(
            "POST {route} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: dudes_floor_session={}\r\nX-Dudes-CSRF: {}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n",
            self.addr, self.addr, self.cookie, self.csrf
        )
    }

    fn post(&self, route: &str, body: &[u8]) -> (u16, Value) {
        let request = [self.head(route, body.len()).as_bytes(), body].concat();
        let (code, _, response) = http(self.addr, &request);
        (
            code,
            if response.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&response).expect("action JSON")
            },
        )
    }

    fn snapshot(&self) -> Value {
        self.open_stream().1
    }

    fn open_stream(&self) -> (TcpStream, Value) {
        let mut stream = TcpStream::connect(self.addr).expect("Floor stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("stream read bound");
        let addr = self.addr;
        let request = format!(
            "GET /floor/stream HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{}\r\nCookie: dudes_floor_session={}\r\n\r\n",
            self.csrf, self.cookie
        );
        stream.write_all(request.as_bytes()).expect("upgrade write");
        assert_eq!(response(&mut stream).0, 101);
        let payload = b"{\"type\":\"floor.subscribe\"}";
        let mut frame = vec![
            0x81,
            0x80 | u8::try_from(payload.len()).unwrap(),
            0,
            0,
            0,
            0,
        ];
        frame.extend_from_slice(payload);
        stream.write_all(&frame).expect("subscribe");
        let snapshot = read_frame(&mut stream);
        (stream, snapshot)
    }
}

fn read_frame(stream: &mut TcpStream) -> Value {
    let mut prefix = [0; 2];
    stream.read_exact(&mut prefix).expect("snapshot frame");
    assert_eq!(prefix[0], 0x81);
    let length = match prefix[1] & 0x7f {
        126 => {
            let mut bytes = [0; 2];
            stream.read_exact(&mut bytes).unwrap();
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).unwrap();
            usize::try_from(u64::from_be_bytes(bytes)).unwrap()
        }
        length => usize::from(length),
    };
    assert!(length <= 1_048_576);
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).expect("snapshot payload");
    serde_json::from_slice(&payload).expect("snapshot JSON")
}

#[test]
fn startup_config_needs_no_chat_key_and_ignores_one_that_is_present() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("config.toml");
    assert!(
        !load_operator_with_options(&path)
            .unwrap()
            .options
            .chat_key_ignored()
    );
    for body in [
        "[floor.chat]\nenabled = true\n",
        "[floor.chat]\nenabled = false\n",
    ] {
        std::fs::write(&path, body).unwrap();
        let startup = load_operator_with_options(&path).unwrap();
        assert!(startup.options.chat_key_ignored());
        assert!(!startup.operator.control.pause_resume());
        assert!(!startup.operator.hire_enabled);
        assert!(!startup.operator.terminal_enabled);
    }
    for bad in [
        "[floor.chat]\nenabled = 'true'\n",
        "[floor.chat]\nenabled = true\nextra = true\n",
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(load_operator_with_options(&path).is_err());
    }
}

#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<String>>>);

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl Subscriber for Warnings {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() == Level::WARN
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &SpanRecord<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[test]
fn a_fresh_daemon_with_no_chat_config_accepts_a_send() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let send = serde_json::to_vec(
        &json!({"request_id":REQUEST_ID,"agent":agent,"text":"no switch needed"}),
    )
    .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &send);
    assert_eq!(code, 200, "{receipt}");
    assert_eq!(receipt["state"], "queued");
    assert_eq!(
        browser.snapshot()["chat"]["pending"][0]["text"],
        "no switch needed"
    );
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn a_config_that_sets_chat_off_still_accepts_a_send_and_warns_once() {
    let fixture = Fixture::new();
    let config = fixture.state.path().join("operator.toml");
    std::fs::write(&config, "[floor.chat]\nenabled = false\n").unwrap();
    let options = load_operator_with_options(&config).unwrap().options;
    let warnings = Warnings::default();
    let (_, mut running, browser) = tracing::subscriber::with_default(warnings.clone(), || {
        let mut settings = fixture.settings();
        settings.floor = Some(FloorSettings {
            assets_dir: PathBuf::from(FLOOR_FIXTURE),
            port: Some(0),
        });
        start_configured(options, None, settings, None)
    });
    let about_chat: Vec<_> = warnings
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|message| message.contains("[floor.chat]"))
        .cloned()
        .collect();
    assert_eq!(
        about_chat,
        ["[floor.chat] in the operator file is ignored: Chat is always on and has no switch"]
    );
    let agent = paused_goal(&fixture, &running);
    let send =
        serde_json::to_vec(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"still accepted"}))
            .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &send);
    assert_eq!(code, 200, "{receipt}");
    assert_eq!(receipt["state"], "queued");
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn queued_send_cancel_and_retries_are_durable_and_attributed() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let send = serde_json::to_vec(
        &json!({"request_id":REQUEST_ID,"agent":agent,"text":" \nhello\tworld \r\n"}),
    )
    .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &send);
    assert_eq!(code, 200);
    let note = receipt["note_lsn"].as_str().expect("note identity");
    assert_eq!(
        receipt,
        json!({"request_id":REQUEST_ID,"state":"queued","agent":agent,"note_lsn":note})
    );
    let snapshot = browser.snapshot();
    assert_eq!(snapshot["chat"]["pending"][0]["text"], "hello\tworld");
    assert_eq!(snapshot["chat"]["pending"][0]["cancellable"], true);
    let cancel =
        serde_json::to_vec(&json!({"request_id":CANCEL_ID,"agent":agent,"note_lsn":note})).unwrap();
    let cancelled = browser.post("/floor/chat/cancel", &cancel);
    assert_eq!(
        cancelled,
        (
            200,
            json!({"request_id":CANCEL_ID,"state":"cancelled","agent":agent,"note_lsn":note})
        )
    );
    assert_eq!(
        browser.post("/floor/chat/send", &send),
        (200, receipt.clone())
    );
    assert_eq!(browser.post("/floor/chat/cancel", &cancel), cancelled);
    let changed =
        serde_json::to_vec(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"different"}))
            .unwrap();
    assert_eq!(
        browser.post("/floor/chat/send", &changed),
        (409, Value::Null)
    );
    assert_eq!(browser.snapshot()["chat"]["pending"], json!([]));
    let notes: Vec<_> = fixture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                Record::BrowserNoteQueued { .. } | Record::BrowserNoteCancelled { .. }
            )
        })
        .collect();
    assert_eq!(notes.len(), 2);
    let Record::BrowserNoteQueued {
        text,
        browser: attribution,
        ..
    } = &notes[0]
    else {
        panic!("queued record")
    };
    assert_eq!(text, "hello\tworld");
    assert_eq!(
        attribution.request_id_digest.bytes(),
        <[u8; 32]>::from(Sha256::digest([0; 32]))
    );
    let encoded = serde_json::to_string(&notes).unwrap();
    assert!(!encoded.contains(REQUEST_ID));
    assert!(!encoded.contains(&browser.cookie));
    assert!(!encoded.contains(&browser.csrf));
    running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "retire fixture agent".into(),
            },
        )))
        .expect("retire paused fixture agent");
    assert_eq!(
        browser.post("/floor/chat/send", &send),
        (200, receipt.clone())
    );
    assert_eq!(browser.post("/floor/chat/cancel", &cancel), cancelled);
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
    let (_, mut running, browser) = start(&fixture);
    assert_eq!(browser.post("/floor/chat/send", &send), (200, receipt));
    assert_eq!(browser.post("/floor/chat/cancel", &cancel), cancelled);
    assert_eq!(browser.snapshot()["chat"]["pending"], json!([]));
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn chat_routes_refuse_oversize_and_expired_sessions_without_waiting_for_body() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    for route in ["/floor/chat/send", "/floor/chat/cancel"] {
        let request = browser.head(route, 16_385);
        assert_eq!(http(browser.addr, request.as_bytes()).0, 413);
        let request = browser.head(route, 16_384).replace(
            &browser.cookie,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );
        assert_eq!(http(browser.addr, request.as_bytes()).0, 403);
    }
    browser.clock.advance(24 * 60 * 60_000);
    assert_eq!(
        http(
            browser.addr,
            browser.head("/floor/chat/send", 16_384).as_bytes()
        )
        .0,
        403
    );
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn retirement_removes_pending_text_and_keeps_the_original_durable_receipt() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let body = serde_json::to_vec(
        &json!({"request_id":REQUEST_ID,"agent":agent,"text":"retired private note"}),
    )
    .unwrap();
    let receipt = browser.post("/floor/chat/send", &body);
    assert_eq!(receipt.0, 200);
    assert_eq!(
        browser.snapshot()["chat"]["pending"][0]["text"],
        "retired private note"
    );
    running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "retire fixture agent".into(),
            },
        )))
        .unwrap();
    assert_eq!(browser.snapshot()["chat"]["pending"], json!([]));
    assert_eq!(browser.post("/floor/chat/send", &body), receipt);
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn exact_chat_paths_and_flat_json_are_the_only_ingress() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let valid =
        serde_json::to_string(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"hello"}))
            .unwrap();
    for path in [
        "/floor/chat/send/",
        "/floor/chat/send?x=1",
        "/floor/chat",
        "/floor/chat/toggle",
        "/floor/chat/cancel/",
    ] {
        assert_eq!(browser.post(path, valid.as_bytes()).0, 400);
    }
    for (index, body) in [
        valid.replace(
            "\"text\":\"hello\"",
            "\"text\":\"hello\",\"text\":\"again\"",
        ),
        valid.replace("\"text\":\"hello\"", "\"text\":{}"),
        valid.replace(
            "\"text\":\"hello\"",
            "\"text\":\"hello\",\"action\":\"run\"",
        ),
        valid.replace("hello", "\\ud800"),
        valid.replace(REQUEST_ID, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB"),
        valid.replace(&agent.to_string(), "01arz3ndektsv4rrffq69g5fav"),
        format!("[\"{REQUEST_ID}\",\"{agent}\",\"hello\"]"),
        format!("{valid} {{}}"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            browser.post("/floor/chat/send", body.as_bytes()).0,
            400,
            "malformed case {index}"
        );
    }
    assert_eq!(browser.post("/floor/chat/send", valid.as_bytes()).0, 200);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteQueued { .. }))
            .count(),
        1
    );
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn pending_admission_reserves_escaped_worst_states_without_eviction() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let text = "\\".repeat(4_096);
    let mut accepted = 0;
    for number in 0..20 {
        let body =
            serde_json::to_vec(&json!({"request_id":request_id(number),"agent":agent,"text":text}))
                .unwrap();
        let (code, receipt) = browser.post("/floor/chat/send", &body);
        if code == 409 {
            assert_eq!(receipt["reason"], "queue_full");
            break;
        }
        assert_eq!(code, 200);
        accepted += 1;
    }
    assert_eq!(accepted, 15);
    let snapshot = browser.snapshot();
    assert_eq!(snapshot["chat"]["pending"].as_array().unwrap().len(), 15);
    assert!(
        serde_json::to_vec(&snapshot["chat"]["pending"])
            .unwrap()
            .len()
            <= 131_072
    );
    assert!(serde_json::to_vec(&snapshot["chat"]).unwrap().len() + 8 <= 344_064);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteQueued { .. }))
            .count(),
        15
    );
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn current_world_refusals_are_fixed_and_cancel_preserves_halt() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let send = |number, agent, reason| {
        let body = serde_json::to_vec(
            &json!({"request_id":request_id(number),"agent":agent,"text":"hello"}),
        )
        .unwrap();
        assert_eq!(
            browser.post("/floor/chat/send", &body),
            (
                409,
                json!({"request_id":request_id(number),"state":"refused","reason":reason})
            )
        );
    };
    send(0, dudes_types::AgentId::NIL, "agent_absent");
    let no_goal = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(no_goal)).unwrap();
    running.handle.ask(Ask::Halt(no_goal)).unwrap();
    send(2, no_goal, "halted");
    running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent: no_goal,
                reason: "retire fixture".into(),
            },
        )))
        .unwrap();
    send(3, no_goal, "agent_retired");
    let agent = paused_goal(&fixture, &running);
    let body =
        serde_json::to_vec(&json!({"request_id":request_id(4),"agent":agent,"text":"held note"}))
            .unwrap();
    let (code, queued) = browser.post("/floor/chat/send", &body);
    assert_eq!(code, 200);
    let note = queued["note_lsn"].as_str().unwrap();
    for (number, target, position) in [(5, agent, "18446744073709551615"), (6, no_goal, note)] {
        let body = serde_json::to_vec(
            &json!({"request_id":request_id(number),"agent":target,"note_lsn":position}),
        )
        .unwrap();
        assert_eq!(
            browser.post("/floor/chat/cancel", &body),
            (
                409,
                json!({"request_id":request_id(number),"state":"refused","reason":"note_unknown"})
            )
        );
    }
    running.handle.ask(Ask::Halt(agent)).unwrap();
    let cancel =
        serde_json::to_vec(&json!({"request_id":request_id(7),"agent":agent,"note_lsn":note}))
            .unwrap();
    assert_eq!(
        browser.post("/floor/chat/cancel", &cancel),
        (
            200,
            json!({"request_id":request_id(7),"state":"cancelled","agent":agent,"note_lsn":note})
        )
    );
    let again =
        serde_json::to_vec(&json!({"request_id":request_id(8),"agent":agent,"note_lsn":note}))
            .unwrap();
    assert_eq!(
        browser.post("/floor/chat/cancel", &again),
        (
            409,
            json!({"request_id":request_id(8),"state":"refused","reason":"note_cancelled"})
        )
    );
    send(9, agent, "halted");
    assert_eq!(browser.snapshot()["chat"]["pending"], json!([]));
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn an_unavailable_provider_refuses_delivery_support_before_append() {
    let fixture = Fixture::new();
    let (_, mut running, _) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
    let empty = dudes_providers::registry::Adapters::new(std::iter::empty::<
        Box<dyn dudes_providers::adapter::Adapter>,
    >());
    let (_, mut running, browser) = start_with_adapters(&fixture, Some(empty));
    let body =
        serde_json::to_vec(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"hello"})).unwrap();
    assert_eq!(
        browser.post("/floor/chat/send", &body),
        (
            409,
            json!({"request_id":REQUEST_ID,"state":"refused","reason":"unsupported_delivery"})
        )
    );
    assert_eq!(browser.snapshot()["chat"]["pending"], json!([]));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteQueued { .. }))
            .count(),
        0
    );
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn durable_receipts_precede_the_stopped_writer_fence() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let body =
        serde_json::to_vec(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"durable note"}))
            .unwrap();
    let receipt = browser.post("/floor/chat/send", &body);
    assert_eq!(receipt.0, 200);
    let original_lsn = browser.snapshot()["projection"]["lsn"].clone();
    browser.failure.short_write_next(1);
    let failing = serde_json::to_vec(
        &json!({"request_id":request_id(1),"agent":agent,"text":"unacknowledged note"}),
    )
    .unwrap();
    assert_eq!(
        browser.post("/floor/chat/send", &failing),
        (503, Value::Null)
    );
    assert_eq!(browser.post("/floor/chat/send", &body), receipt);
    let fresh = serde_json::to_vec(
        &json!({"request_id":request_id(2),"agent":agent,"text":"fresh note behind fence"}),
    )
    .unwrap();
    assert_eq!(browser.post("/floor/chat/send", &fresh), (503, Value::Null));
    assert_eq!(browser.snapshot()["projection"]["lsn"], original_lsn);
    assert_eq!(
        browser.snapshot()["chat"]["pending"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

#[test]
fn live_chat_replacements_include_empty_arrays_at_the_same_projection_lsn() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = paused_goal(&fixture, &running);
    let (mut stream, snapshot) = browser.open_stream();
    assert_eq!(snapshot["chat"]["pending"], json!([]));
    let body =
        serde_json::to_vec(&json!({"request_id":REQUEST_ID,"agent":agent,"text":"live note"}))
            .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &body);
    assert_eq!(code, 200);
    let queued = chat_replacement(&mut stream);
    assert_eq!(queued["pending"][0]["text"], "live note");
    let cancel = serde_json::to_vec(
        &json!({"request_id":CANCEL_ID,"agent":agent,"note_lsn":receipt["note_lsn"]}),
    )
    .unwrap();
    assert_eq!(browser.post("/floor/chat/cancel", &cancel).0, 200);
    assert_eq!(
        chat_replacement(&mut stream),
        json!({"enabled":true,"max_text_bytes":4096,"pending":[],"turns":[],"context_sent":[]})
    );
    drop(stream);
    assert_eq!(
        running.stop(duded::ExitCause::Signal),
        duded::ExitCause::Signal
    );
}

fn chat_replacement(stream: &mut TcpStream) -> Value {
    let mut prior = None;
    for _ in 0..32 {
        let frame = read_frame(stream);
        if frame["type"] != "floor.delta" {
            continue;
        }
        if let Some(chat) = frame.get("chat") {
            assert_eq!(Some(&frame["delta"]["lsn"]), prior.as_ref());
            return chat.clone();
        }
        prior = Some(frame["delta"]["lsn"].clone());
    }
    panic!("Chat replacement was not published within the finite frame prefix");
}

#[test]
fn managed_chat_accepts_first_message_without_a_goal() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(agent)).unwrap();
    let body =
        serde_json::to_vec(&json!({"request_id": REQUEST_ID, "agent": agent, "text": "Hello"}))
            .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &body);
    assert_eq!(code, 200);
    assert_eq!(
        receipt["state"], "queued",
        "first message must be durably accepted: {receipt}"
    );
    assert_eq!(browser.post("/floor/chat/send", &body), (200, receipt));
    let snapshot = browser.snapshot();
    assert_eq!(snapshot["chat"]["pending"][0]["text"], "Hello");
    running.stop(duded::ExitCause::Signal);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|entry| matches!(entry, Record::GoalSet { .. }))
    );
}

#[cfg(unix)]
#[path = "common/native_chat_host.rs"]
mod native_chat_host;

#[cfg(unix)]
fn start_real_conversation(
    fixture: &Fixture,
) -> (common::Running, Browser, PathBuf, Arc<ScriptedRunner>) {
    use dudes_providers::claude::{ClaudeAdapter, config::ClaudeConfig};
    use dudes_providers::launch::EnvVar;
    let work = fixture.state.path().join("conversation-provider");
    std::fs::create_dir(&work).unwrap();
    for run in [1, 2] {
        std::fs::write(work.join(format!("release.{run}")), b"").unwrap();
    }
    let command = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/two-roles/bin/claude")
        .canonicalize()
        .unwrap();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.daemon_env = ["PATH", "TMPDIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(name, value)))
        .collect();
    settings
        .daemon_env
        .push(EnvVar::new("DUDES_EXAMPLE_WORK", work.as_os_str()));
    let children = duded::ChildEnds::new();
    let clock = Arc::new(TestClock::new());
    let predicates = Arc::new(ScriptedRunner::scripted([]));
    let seams = duded::settings::Seams {
        host: native_chat_host::owned_chat_host(Arc::clone(&children)),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::clone(&predicates),
        clock: Arc::clone(&clock),
        rng: common::FixtureRng::from_rng(&settings, dudes_supervisor::testing::FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(ClaudeAdapter::new(ClaudeConfig {
            command: command.into_os_string(),
            ..ClaudeConfig::default()
        })) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect(),
        children,
        witness: duded::startup::Witness::default(),
    };
    let daemon = duded::boot_with_options(&settings, seams, RunOptions::default()).unwrap();
    let (_, running, browser) = connect_browser(settings, daemon, clock);
    (running, browser, work, predicates)
}

#[test]
#[cfg(unix)]
fn conversation_replies_once_then_resumes_confirmed_session_for_follow_up() {
    let fixture = Fixture::new();
    let (mut running, browser, work, predicates) = start_real_conversation(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    for (number, message) in [
        (0, "Hello, remember the word amber"),
        (1, "What word did I give you?"),
    ] {
        let body = serde_json::to_vec(
            &json!({"request_id":request_id(number),"agent":agent,"text":message}),
        )
        .unwrap();
        let receipt = browser.post("/floor/chat/send", &body);
        assert_eq!(receipt.0, 200);
        assert_eq!(receipt.1["state"], "queued");
        fixture.await_record("finite turn ended", |record| match record {
            Record::TurnEnded { turn, .. }
                if fixture
                    .records()
                    .iter()
                    .filter(|r| matches!(r, Record::TurnEnded { .. }))
                    .count()
                    == number + 1 =>
            {
                Some(*turn)
            }
            _ => None,
        });
        assert_eq!(browser.post("/floor/chat/send", &body), receipt);
    }
    running.stop(duded::ExitCause::Signal);
    let projection =
        dudes_journal::Projections::open(&fixture.state.path().join(duded::boot::PROJECTION_FILE))
            .unwrap();
    assert_eq!(
        projection.agent(agent).unwrap().unwrap().status,
        dudes_types::AgentStatus::Idle
    );
    let chat = projection.chat().unwrap();
    assert_eq!(chat.turns.len(), 2);
    assert!(
        chat.turns
            .iter()
            .all(|turn| turn.reply.is_some() && turn.truncated == Some(false))
    );
    let records = fixture.records();
    let issued: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::ConversationTurnIssued { turn, session, .. } => Some((*turn, session)),
            _ => None,
        })
        .collect();
    assert_eq!(issued.len(), 2);
    assert!(issued[0].1.is_some());
    assert_eq!(issued[0].1, issued[1].1);
    assert_eq!(predicates.runs(), 0);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::GoalSet { .. } | Record::LoopDecided { .. }))
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::SessionLearned { .. }))
            .count(),
        2
    );
    let files: Vec<_> = std::fs::read_dir(work.join("transcripts"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 1);
    let transcript = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    assert!(transcript.contains("--session-id"));
    assert!(transcript.contains("--resume"));
    assert!(!work.join("started.3").exists());
}

#[test]
fn conversation_captures_the_no_goal_revision_and_remains_cancellable_when_stale() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(agent)).unwrap();
    let body = serde_json::to_vec(
        &json!({"request_id": REQUEST_ID, "agent": agent, "text": "Before the goal"}),
    )
    .unwrap();
    let (code, receipt) = browser.post("/floor/chat/send", &body);
    assert_eq!(code, 200);
    common::set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let snapshot = browser.snapshot();
    assert_eq!(snapshot["chat"]["pending"][0]["reason"], "goal_changed");
    assert_eq!(snapshot["chat"]["pending"][0]["cancellable"], true);
    let body = serde_json::to_vec(
        &json!({"request_id": CANCEL_ID, "agent": agent, "note_lsn": receipt["note_lsn"]}),
    )
    .unwrap();
    assert_eq!(
        browser.post("/floor/chat/cancel", &body).1["state"],
        "cancelled"
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::ConversationTurnIssued { .. }))
    );
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn conversation_refuses_a_zero_agent_budget_before_issuance() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let mut hire = common::hire_params(fixture.state.path());
    hire.budget.usd_per_day = Some(dudes_types::Usd::ZERO);
    let duded::ports::Answered::Hired(hired) =
        running.handle.ask(Ask::Hire(Box::new(hire))).unwrap()
    else {
        panic!("hire")
    };
    let agent = hired.agent;
    let body = serde_json::to_vec(
        &json!({"request_id": REQUEST_ID, "agent": agent, "text": "Please reply"}),
    )
    .unwrap();
    assert_eq!(browser.post("/floor/chat/send", &body).1["state"], "queued");
    fixture.await_record("finite budget refusal", |record| {
        matches!(
            record,
            Record::ConversationTurnDecided {
                decision: dudes_proto::NoteAdmission::Exhausted {
                    reason: dudes_proto::ExhaustedReason::AgentBudget
                },
                ..
            }
        )
        .then_some(())
    });
    assert_eq!(browser.snapshot()["chat"]["pending"][0]["reason"], "budget");
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::ConversationTurnIssued { .. }))
    );
    let snapshot = browser.snapshot();
    let row = snapshot["projection"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == agent.to_string())
        .unwrap();
    assert_eq!(row["status"], "exhausted");
    running.stop(duded::ExitCause::Signal);
}

fn conversation_answer_end(first: dudes_types::TurnId) -> dudes_supervisor::Input {
    dudes_supervisor::Input::ChildExited {
        turn: first,
        elapsed_ms: Some(100),
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: std::collections::BTreeMap::default(),
        outcome: dudes_proto::TurnOutcome::Completed,
        last_message: None,
        final_reply: None,
    }
}

#[test]
fn conversation_answer_binds_question_and_precedes_unrelated_messages() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start_with_grants(&fixture, true, None, None);
    let agent = common::hire(&running.handle, fixture.state.path());
    let send = |number, text| {
        serde_json::to_vec(&json!({"request_id":request_id(number),"agent":agent,"text":text}))
            .unwrap()
    };
    assert_eq!(
        browser
            .post("/floor/chat/send", &send(0, "Help me choose"))
            .0,
        200
    );
    let first = fixture.await_record("first conversation spawn", |record| match record {
        Record::TurnSpawned { turn, .. } => Some(*turn),
        _ => None,
    });
    running
        .handle
        .ask(Ask::Ask {
            agent,
            text: "Which project?".into(),
        })
        .unwrap();
    let question_lsn = browser.snapshot()["projection"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == agent.to_string())
        .unwrap()["question_lsn"]
        .clone();
    assert!(question_lsn.is_string());
    assert_eq!(
        browser
            .post("/floor/chat/send", &send(1, "Unrelated later message"))
            .0,
        200
    );
    let answer = serde_json::to_vec(&json!({"request_id":request_id(2),"agent":agent,"question_lsn":question_lsn,"text":"This project"})).unwrap();
    let stale = serde_json::to_vec(&json!({"request_id":request_id(3),"agent":agent,"question_lsn":"18446744073709551615","text":"wrong target"})).unwrap();
    assert_eq!(
        browser.post("/floor/answer", &stale).1["reason"],
        "question_closed"
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::ConversationAnswerQueued { .. }))
    );
    let receipt = browser.post("/floor/answer", &answer);
    assert_eq!(receipt.0, 200);
    assert_eq!(receipt.1["state"], "answered");
    assert_eq!(browser.post("/floor/answer", &answer), receipt);
    let queued = fixture.await_record("compound answer authority", |record| match record {
        Record::ConversationAnswerQueued { origin, task, .. } => Some((*origin, task.clone())),
        _ => None,
    });
    assert_eq!(queued.0.turn, first);
    assert_eq!(
        queued.1,
        dudes_proto::conversation_answer_task("Which project?", "This project")
    );
    common::end_turn_with_native_payload(
        &fixture,
        browser.host.as_ref().expect("owned mock child host"),
        conversation_answer_end(first),
    );
    fixture.await_record("answer continuation", |record| match record {
        Record::ConversationTurnIssued { turn, task, .. } if *turn != first => Some(task.clone()),
        _ => None,
    });
    let tasks: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::ConversationTurnIssued { task, .. } => Some(task),
            _ => None,
        })
        .collect();
    assert_eq!(tasks, ["Help me choose".to_owned(), queued.1]);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::ConversationAnswerQueued { .. }))
            .count(),
        1
    );
    running.stop(duded::ExitCause::Signal);
}

fn conversation_request(
    browser: &Browser,
    agent: dudes_types::AgentId,
    number: usize,
    text: &str,
) -> (u16, Value) {
    browser.post(
        "/floor/chat/send",
        &serde_json::to_vec(&json!({"request_id":request_id(number),"agent":agent,"text":text}))
            .unwrap(),
    )
}

fn conversation_issued(
    fixture: &Fixture,
    task_text: &str,
) -> (dudes_types::TurnId, Option<dudes_types::SessionId>) {
    let issued = fixture.await_record("finite issue", |record| match record {
        Record::ConversationTurnIssued {
            turn,
            session,
            task,
            ..
        } if task == task_text => Some((*turn, session.clone())),
        _ => None,
    });
    fixture.await_record("finite spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == issued.0).then_some(())
    });
    issued
}

fn end_conversation(
    browser: &Browser,
    fixture: &Fixture,
    turn: dudes_types::TurnId,
    outcome: dudes_proto::TurnOutcome,
) {
    let code = i32::from(outcome != dudes_proto::TurnOutcome::Completed);
    common::end_turn_with_native_payload(
        fixture,
        browser.host.as_ref().expect("owned mock child host"),
        dudes_supervisor::Input::ChildExited {
            turn,
            elapsed_ms: Some(6_000),
            exit: dudes_proto::ExitStatus {
                code: Some(code),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(
                dudes_types::Usage::ZERO,
            )),
            model_usage: std::collections::BTreeMap::new(),
            outcome,
            last_message: None,
            final_reply: None,
        },
    );
    fixture.await_record("finite end", |record| {
        matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn).then_some(())
    });
}

#[test]
fn conversation_context_loss_requires_exact_reset_and_keeps_old_queue_stale() {
    conversation_context_loss_after(dudes_proto::TurnOutcome::Failed {
        reason: "fixture resume refused".into(),
    });
}

#[test]
fn conversation_no_result_before_confirmation_requires_exact_context_reset() {
    conversation_context_loss_after(dudes_proto::TurnOutcome::NoResult);
}

fn await_conversation_end_comparison(
    fixture: &Fixture,
    agent: dudes_types::AgentId,
    turn: dudes_types::TurnId,
) {
    let end_lsn = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::TurnEnded {
                agent: ended,
                turn: seen,
                ..
            } if ended == agent && seen == turn => Some(lsn),
            _ => None,
        })
        .expect("the exact conversation turn ended");
    fixture.await_record("the exact conversation end comparison", |record| {
        matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed {
                agent: owner, turn: observed, ..
            },
            settlement:
                dudes_proto::InstructionSettlement::Compared {
                    end: dudes_proto::InstructionEndBoundary::ManagedTurn {
                        turn: ended, end_lsn: seen
                    }, ..
                }
                | dudes_proto::InstructionSettlement::Unknown {
                    end: Some(dudes_proto::InstructionEndBoundary::ManagedTurn {
                        turn: ended, end_lsn: seen
                    }), ..
                },
            ..
        } if *owner == agent && *observed == turn && *ended == turn && *seen == end_lsn)
        .then_some(())
    });
}

fn conversation_context_loss_after(outcome: dudes_proto::TurnOutcome) {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    assert_eq!(conversation_request(&browser, agent, 0, "first").0, 200);
    let (first, session) = conversation_issued(&fixture, "first");
    running
        .handle
        .input(dudes_supervisor::Input::SessionLearned {
            turn: first,
            session: session.clone().unwrap(),
        });
    end_conversation(
        &browser,
        &fixture,
        first,
        dudes_proto::TurnOutcome::Completed,
    );
    let second_receipt = conversation_request(&browser, agent, 1, "second");
    assert_eq!(second_receipt.0, 200);
    let (second, resumed) = conversation_issued(&fixture, "second");
    assert_eq!(resumed, session);
    let old_queue = conversation_request(&browser, agent, 2, "queued before loss");
    assert_eq!(old_queue.0, 200);
    end_conversation(&browser, &fixture, second, outcome);
    await_conversation_end_comparison(&fixture, agent, second);
    running.handle.doctor().unwrap();
    assert_eq!(
        conversation_request(&browser, agent, 1, "second"),
        second_receipt
    );
    let count = fixture.records().len();
    assert_eq!(
        conversation_request(&browser, agent, 3, "fresh requested"),
        (
            409,
            json!({"state":"context_lost","request_id":request_id(3),"agent":agent,"turn_id":second})
        )
    );
    assert_eq!(fixture.records().len(), count);
    let reset = |number, turn| {
        serde_json::to_vec(&json!({"request_id":request_id(number),"agent":agent,"text":"fresh requested","context_reset_turn":turn})).unwrap()
    };
    assert_eq!(
        browser.post("/floor/chat/send", &reset(4, first)).1["reason"],
        "context_changed"
    );
    assert_eq!(fixture.records().len(), count);
    let receipt = browser.post("/floor/chat/send", &reset(5, second));
    assert_eq!(receipt.0, 200);
    let (third, new_session) = conversation_issued(&fixture, "fresh requested");
    assert_ne!(new_session, session);
    assert_eq!(browser.post("/floor/chat/send", &reset(5, second)), receipt);
    assert_eq!(
        browser.post("/floor/chat/send", &reset(6, second)).1["reason"],
        "context_changed"
    );
    let snapshot = browser.snapshot();
    let stale = snapshot["chat"]["pending"]
        .as_array()
        .unwrap()
        .iter()
        .find(|note| note["note_lsn"] == old_queue.1["note_lsn"])
        .unwrap();
    assert_eq!(stale["reason"], "context_changed");
    assert_eq!(stale["cancellable"], true);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::ConversationTurnIssued { .. }))
            .count(),
        3
    );
    end_conversation(
        &browser,
        &fixture,
        third,
        dudes_proto::TurnOutcome::Completed,
    );
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::ConversationTurnIssued { task, .. } if task == "queued before loss")));
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn conversation_daily_budget_refresh_reconsiders_a_pending_message() {
    let fixture = Fixture::new();
    let (_, mut running, browser) = start(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    assert_eq!(
        conversation_request(&browser, agent, 0, "first daily turn").0,
        200
    );
    let (turn, session) = conversation_issued(&fixture, "first daily turn");
    running
        .handle
        .input(dudes_supervisor::Input::SessionLearned {
            turn,
            session: session.unwrap(),
        });
    common::end_turn_with_native_payload(
        &fixture,
        browser.host.as_ref().expect("owned mock child host"),
        dudes_supervisor::Input::ChildExited {
            turn,
            elapsed_ms: Some(6_000),
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(
                dudes_types::Usage {
                    usd: Some(dudes_types::Usd::from_dollars(10)),
                    ..dudes_types::Usage::ZERO
                },
            )),
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
            final_reply: None,
        },
    );
    fixture.await_record("daily spend", |record| {
        matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn).then_some(())
    });
    assert_eq!(
        conversation_request(&browser, agent, 1, "next day reply").0,
        200
    );
    assert_eq!(browser.snapshot()["chat"]["pending"][0]["reason"], "budget");
    browser.clock.advance(86_400_000);
    running.handle.input(dudes_supervisor::Input::Tick);
    conversation_issued(&fixture, "next day reply");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::ConversationTurnIssued { .. }))
            .count(),
        2
    );
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn conversation_repeated_fast_failure_keeps_the_existing_hold_guard() {
    let fixture = Fixture::new();
    let (settings, mut running, browser) = start(&fixture);
    let agent = common::hire(&running.handle, fixture.state.path());
    for (number, text) in [(0, "first failing turn"), (1, "second failing turn")] {
        assert_eq!(conversation_request(&browser, agent, number, text).0, 200);
        let (turn, _) = conversation_issued(&fixture, text);
        common::end_turn_with_native_payload(
            &fixture,
            browser.host.as_ref().expect("owned mock child host"),
            dudes_supervisor::Input::ChildExited {
                turn,
                elapsed_ms: Some(100),
                exit: dudes_proto::ExitStatus {
                    code: Some(1),
                    signal: None,
                },
                usage: None,
                model_usage: std::collections::BTreeMap::new(),
                outcome: dudes_proto::TurnOutcome::Failed {
                    reason: "owned fixture failure".into(),
                },
                last_message: None,
                final_reply: None,
            },
        );
        fixture.await_record("failed finite turn", |record| {
            matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn).then_some(())
        });
    }
    fixture.await_record("existing rapid failure hold", |record| {
        matches!(
            record,
            Record::AgentHeld {
                cause: dudes_proto::AutomaticHold::RepeatedFastFailure { .. },
                ..
            }
        )
        .then_some(())
    });
    assert_eq!(
        conversation_request(&browser, agent, 2, "must wait for recovery").0,
        200
    );
    assert_eq!(browser.snapshot()["chat"]["pending"][0]["reason"], "hold");
    running.stop(duded::ExitCause::Signal);
    let projected =
        dudes_journal::Projections::open(&settings.state_dir.join(duded::boot::PROJECTION_FILE))
            .unwrap();
    let day = dudes_supervisor::Clock::now(browser.clock.as_ref()).utc_day();
    let spend = projected.conversation_spend_views(agent, day).unwrap();
    assert_eq!(spend.agent_unknown_ends_today, 2);
    assert_eq!(spend.floor_unknown_ends_today, 2);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::ConversationTurnIssued { .. }))
            .count(),
        2
    );
}

fn waiting_conversation(
    fixture: &Fixture,
    running: &common::Running,
    browser: &Browser,
) -> (dudes_types::AgentId, String) {
    let agent = common::hire(&running.handle, fixture.state.path());
    assert_eq!(
        conversation_request(browser, agent, 0, "A task needing clarification").0,
        200
    );
    let (turn, _) = conversation_issued(fixture, "A task needing clarification");
    running
        .handle
        .ask(Ask::Ask {
            agent,
            text: "Which branch?".into(),
        })
        .unwrap();
    end_conversation(browser, fixture, turn, dudes_proto::TurnOutcome::Completed);
    let question = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record, Record::ConversationQuestion { .. }).then_some(lsn.0.to_string())
        })
        .unwrap();
    (agent, question)
}

fn operator_answer(
    settings: &duded::Settings,
    agent: dudes_types::AgentId,
    text: &str,
) -> dudes_proto::rpc::Response {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .unwrap();
    Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            Method::AgentAnswer,
            Some(json!({"agent":agent,"text":text})),
            OperatorCredential::new(credential).unwrap(),
        )
        .unwrap()
}

#[test]
fn conversation_local_answer_normalizes_spaces_and_refuses_controls_before_append() {
    let fixture = Fixture::new();
    let (settings, mut running, browser) = start_with_grants(
        &fixture,
        true,
        None,
        Some(common::ScriptedColumn::new(
            dudes_proto::PeerProvenance::NoRecordedAgent,
        )),
    );
    let (agent, question) = waiting_conversation(&fixture, &running, &browser);
    let (turn, end_lsn) = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::TurnEnded {
                agent: ended, turn, ..
            } if ended == agent => Some((turn, lsn)),
            _ => None,
        })
        .expect("the exact conversation turn ended");
    fixture.await_record("the exact conversation end comparison", |record| {
        matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed {
                agent: owner, turn: observed, ..
            },
            settlement:
                dudes_proto::InstructionSettlement::Compared {
                    end: dudes_proto::InstructionEndBoundary::ManagedTurn {
                        turn: ended, end_lsn: seen
                    }, ..
                }
                | dudes_proto::InstructionSettlement::Unknown {
                    end: Some(dudes_proto::InstructionEndBoundary::ManagedTurn {
                        turn: ended, end_lsn: seen
                    }), ..
                },
            ..
        } if *owner == agent && *observed == turn && *ended == turn && *seen == end_lsn)
        .then_some(())
    });
    running.handle.doctor().unwrap();
    let before = fixture.records().len();
    let invalid_browser = serde_json::to_vec(&json!({"request_id":request_id(7),"agent":agent,"question_lsn":question,"text":"use\rmain"})).unwrap();
    assert_eq!(browser.post("/floor/answer", &invalid_browser).0, 400);
    assert_eq!(fixture.records().len(), before);
    let refused = operator_answer(&settings, agent, "use\rmain");
    assert_eq!(
        refused.error.unwrap().code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code()
    );
    assert_eq!(fixture.records().len(), before);
    assert_eq!(
        browser.snapshot()["projection"]["agents"][0]["question_lsn"],
        question
    );
    assert!(operator_answer(&settings, agent, " main ").error.is_none());
    fixture.await_record("normalized finite local answer", |record| {
        matches!(record, Record::ConversationAnswerQueued { text, .. } if text == "main")
            .then_some(())
    });
    let chat = &browser.snapshot()["chat"];
    assert!(
        chat["pending"]
            .as_array()
            .unwrap()
            .iter()
            .chain(chat["turns"].as_array().unwrap())
            .any(|row| row["text"] == "main")
    );
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn ordinary_goal_local_answer_keeps_its_original_whitespace_and_controls() {
    let fixture = Fixture::new();
    let (settings, mut running, _) = start_with_grants(
        &fixture,
        true,
        None,
        Some(common::ScriptedColumn::new(
            dudes_proto::PeerProvenance::NoRecordedAgent,
        )),
    );
    let agent = paused_goal(&fixture, &running);
    running
        .handle
        .ask(Ask::Ask {
            agent,
            text: "Which branch?".into(),
        })
        .unwrap();
    assert!(
        operator_answer(&settings, agent, " use\rmain ")
            .error
            .is_none()
    );
    fixture.await_record("unchanged ordinary answer", |record| {
        matches!(record, Record::HumanAnswer { text, .. } if text == " use\rmain ").then_some(())
    });
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::ConversationAnswerQueued { .. }))
    );
    running.stop(duded::ExitCause::Signal);
}

fn with_owned_chrome<T>(action: impl FnOnce(&std::path::Path) -> T) -> T {
    let chrome = PathBuf::from(
        std::env::var_os("DUDES_INTEGRATION_BROWSER_CHROME").unwrap_or_else(|| {
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into()
        }),
    );
    assert!(chrome.is_absolute() && chrome.is_file());
    let profile = tempfile::tempdir().unwrap();
    let marker = profile.path().join("DevToolsActivePort");
    let mut directory = std::ffi::OsString::from("--user-data-dir=");
    directory.push(profile.path());
    let spec = dudes_providers::launch::LaunchSpec {
        argv: vec![
            chrome.into_os_string(),
            "--headless=new".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-background-networking".into(),
            "--remote-debugging-port=0".into(),
            "--remote-debugging-address=127.0.0.1".into(),
            directory,
            "about:blank".into(),
        ],
        env: duded::predicate::allowlisted(std::env::vars_os()),
        cwd: profile.path().to_owned(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    };
    let owner = Arc::new(dudes_host::OneShot::new());
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| owner.run_discarding_output(&spec, Duration::from_secs(120)));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !marker.is_file() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned browser marker not ready"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            action(&marker)
        }));
        let port = std::fs::read_to_string(&marker).ok().and_then(|text| {
            text.lines()
                .next()
                .and_then(|port| port.parse::<u16>().ok())
        });
        owner.sweep();
        let stopped = waiter.join().expect("owned browser waiter joins");
        assert!(stopped.is_ok(), "owned browser shutdown completes");
        assert_eq!(owner.in_flight(), 0);
        if let Some(port) = port {
            let endpoint = SocketAddr::from(([127, 0, 0, 1], port));
            assert!(
                TcpStream::connect_timeout(&endpoint, Duration::from_millis(100)).is_err(),
                "owned debug listener gone after descendant sweep and reap"
            );
        }
        match result {
            Ok(value) => value,
            Err(error) => std::panic::resume_unwind(error),
        }
    })
}

fn assert_browser_note_pair(
    entries: &[(dudes_types::Lsn, dudes_proto::Entry)],
    agent: dudes_types::AgentId,
    queued: &Value,
    cancelled: &Value,
    expected_text: &str,
) {
    let note = dudes_types::Lsn(queued.as_str().unwrap().parse().unwrap());
    assert_eq!(queued, cancelled);
    let submissions: Vec<_> = entries
        .iter()
        .filter(|(_, entry)| {
            matches!(entry.record,
        Record::BrowserNoteQueued { agent: who, .. } if who == agent)
        })
        .collect();
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].0, note);
    assert!(
        matches!(&submissions[0].1.record, Record::BrowserNoteQueued { text, .. } if text == expected_text)
    );
    let cancellations: Vec<_> = entries.iter().filter(|(_, entry)| matches!(entry.record,
        Record::BrowserNoteCancelled { agent: who, note_lsn, .. } if who == agent && note_lsn == note)).collect();
    assert_eq!(cancellations.len(), 1);
    assert!(cancellations[0].0 > note);
}

#[test]
#[ignore = "needs a browser driver script and a Node runtime supplied through the environment"]
fn owned_authenticated_browser_chat_smoke() {
    let script = PathBuf::from(
        std::env::var_os("DUDES_INTEGRATION_BROWSER_SCRIPT").expect("browser driver script path"),
    );
    let node = PathBuf::from(
        std::env::var_os("DUDES_INTEGRATION_BROWSER_NODE").expect("owned Node runtime path"),
    );
    assert!(script.is_absolute() && script.is_file());
    assert!(node.is_absolute() && node.is_file());
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../floor/dist")),
        port: Some(0),
    });
    settings.floor_control = duded::settings::FloorControl::On {
        goal: false,
        retire: false,
        renew: false,
    };
    let (_, mut running, browser) = start_configured(RunOptions::default(), None, settings, None);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let paused = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Pause(paused))
        .expect("owned paused fixture");
    common::set_goal(
        &running.handle,
        goal_params(paused, &[common::true_program()]),
    );
    let timeout_spec = dudes_providers::launch::LaunchSpec {
        argv: vec![
            node.as_os_str().to_owned(),
            "-e".into(),
            "setInterval(() => {}, 1000)".into(),
        ],
        env: duded::predicate::allowlisted(std::env::vars_os()),
        cwd: fixture.state.path().to_owned(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    };
    with_owned_chrome(|_| {
        let timed = dudes_host::OneShot::new()
            .run(&timeout_spec, Duration::from_millis(200))
            .unwrap();
        assert!(matches!(
            timed,
            dudes_host::OneShotOutcome::Finished {
                timed_out: true,
                ..
            }
        ));
    });
    let result = with_owned_chrome(|marker| {
        let input = serde_json::to_vec(&json!({"baseUrl": format!("http://{}", browser.addr),
            "cookie": browser.cookie, "csrf": browser.csrf, "agentId": agent, "agentName": "Hector",
            "agentPausedId": paused, "agentPausedName": "Hector", "devToolsPath": marker}))
        .unwrap();
        let spec = dudes_providers::launch::LaunchSpec {
            argv: vec![node.into_os_string(), script.into_os_string()],
            env: duded::predicate::allowlisted(std::env::vars_os()),
            cwd: fixture.state.path().to_owned(),
            stdin: dudes_providers::launch::LaunchInput::WriteThenClose(input),
        };
        dudes_host::OneShot::new().run(&spec, Duration::from_secs(90))
    });
    running.stop(duded::ExitCause::ShutdownRpc);
    let dudes_host::OneShotOutcome::Finished {
        exit: Some(0),
        output_tail,
        timed_out: false,
    } = result.expect("owned browser driver executes")
    else {
        panic!("owned browser driver failed");
    };
    let proof: Value =
        serde_json::from_str(&output_tail).expect("driver returns only sanitized proof JSON");
    assert_eq!(proof["sent"], true);
    assert_eq!(proof["cancelled"], true);
    let entries = fixture.entries();
    assert_browser_note_pair(
        &entries,
        agent,
        &proof["queued_lsn"],
        &proof["cancelled_lsn"],
        "Owned integrated ordinary browser smoke",
    );
    assert_browser_note_pair(
        &entries,
        paused,
        &proof["paused"]["queued"],
        &proof["paused"]["cancelled"],
        "Owned paused delivery smoke",
    );
}
