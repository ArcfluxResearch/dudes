//! Through a booted daemon: the Floor's count of goals first finished on the daemon's local
//! day rests on an observed pass, survives goal replacement and a restart, follows the daemon's
//! zone and clock across a date boundary and a zone change, and journals nothing of its own.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono_tz::Tz;
use duded::floor_stream::FloorStreamFault;
use duded::local_day::{LocalDays, window_in};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_rpc::Client;
use dudes_supervisor::Input;
use dudes_types::{AgentId, GoalId, Timestamp, TurnId};
use serde_json::Value;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, fails, goal_params, passes, seams};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

/// 2026-10-07T08:00:00Z, which is 21:00 on 7 October in Auckland and 01:00 in Los Angeles.
const AUCKLAND_NINE_PM: i64 = 1_791_360_000_000;

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve loopback port")
        .local_addr()
        .expect("reserved address")
        .port()
}

fn settings(fixture: &Fixture, port: u16) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(port),
    });
    settings
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("HTTP head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

fn session(settings: &duded::Settings, addr: SocketAddr) -> (String, String) {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("the daemon minted an operator credential"),
    )
    .expect("a well formed credential");
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let response = client
        .call_as_operator(Method::FloorSessionIssue, None, credential)
        .expect("floor.session.issue is answered");
    let issued: SessionIssueResult =
        serde_json::from_value(response.result.expect("issue result")).expect("issue result");
    let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let bootstrap = issued.bootstrap;
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{bootstrap}",
        bootstrap.len()
    );
    stream.write_all(request.as_bytes()).expect("exchange");
    let head = read_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("session cookie")
        .to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("body length")
        .parse()
        .expect("numeric length");
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).expect("session body");
    let json: Value = serde_json::from_slice(&body).expect("session JSON");
    (
        cookie,
        json["csrf"].as_str().expect("CSRF nonce").to_owned(),
    )
}

/// A browser socket subscribed to the Floor stream.
struct Browser {
    stream: TcpStream,
}

impl Browser {
    fn subscribe(addr: SocketAddr, cookie: &str, csrf: &str) -> (Self, Value) {
        let mut stream = TcpStream::connect(addr).expect("connect to the Floor gateway");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let request = format!(
            "GET /floor/stream HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).expect("upgrade");
        let head = read_head(&mut stream);
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        let mut browser = Self { stream };
        let subscribe = br#"{"type":"floor.subscribe"}"#;
        let mask = [1_u8, 2, 3, 4];
        let mut frame = vec![0x81, 0x80 | u8::try_from(subscribe.len()).expect("short")];
        frame.extend_from_slice(&mask);
        frame.extend(
            subscribe
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        browser.stream.write_all(&frame).expect("subscribe frame");
        let snapshot = browser.next(Duration::from_secs(5)).expect("a snapshot");
        assert_eq!(snapshot["type"], "floor.snapshot", "{snapshot}");
        (browser, snapshot)
    }

    /// The next server message, or `None` once `bound` has passed with nothing on the socket.
    fn next(&mut self, bound: Duration) -> Option<Value> {
        self.stream
            .set_read_timeout(Some(bound))
            .expect("read timeout");
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
        assert_eq!(head[0] & 0x0f, 1, "server emits text frames here");
        let length = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0_u8; 2];
                self.stream.read_exact(&mut bytes).expect("u16 length");
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0_u8; 8];
                self.stream.read_exact(&mut bytes).expect("u64 length");
                usize::try_from(u64::from_be_bytes(bytes)).expect("fits")
            }
        };
        self.stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut body = vec![0_u8; length];
        self.stream.read_exact(&mut body).expect("frame body");
        Some(serde_json::from_slice(&body).expect("server JSON"))
    }

    /// Read until a message satisfies `wanted`, skipping heartbeats and unrelated deltas.
    fn until(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(message) = self.next(Duration::from_millis(500))
                && wanted(&message)
            {
                return message;
            }
        }
        panic!("no {what} within 15s");
    }
}

fn total_of(message: &Value) -> Option<&str> {
    message["finished_today"]["total"].as_str()
}

/// The day source over a test zone and a clock of its own, both of which the test can move. The
/// daemon's clock stays frozen, because the Floor session limits and every journal stamp read it
/// too; only the instant the day is read at moves here.
struct Day {
    zone: Arc<Mutex<Tz>>,
    now: Arc<AtomicI64>,
}

impl Day {
    fn new(zone: Tz) -> Self {
        Self {
            zone: Arc::new(Mutex::new(zone)),
            now: Arc::new(AtomicI64::new(AUCKLAND_NINE_PM)),
        }
    }

    fn source(&self) -> LocalDays {
        let zone = Arc::clone(&self.zone);
        let now = Arc::clone(&self.now);
        LocalDays::from_fn(move || {
            let tz = *zone.lock().unwrap_or_else(PoisonError::into_inner);
            window_in(&tz, Timestamp::from_unix_millis(now.load(Ordering::SeqCst)))
        })
    }
}

struct Boot {
    running: common::Running,
    host: SharedHost,
    addr: SocketAddr,
    settings: duded::Settings,
}

fn boot_with(
    fixture: &Fixture,
    clock: &Arc<TestClock>,
    runner: &Arc<ScriptedRunner>,
    day: &Day,
) -> Boot {
    let settings = settings(fixture, free_port());
    let fault = FloorStreamFault::default().without_heartbeat();
    fault.use_local_days(day.source());
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let parts = seams(
        &settings,
        host.clone(),
        Arc::clone(runner),
        Arc::clone(clock),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&parts.children));
    let daemon = boot::boot_with_floor_faults(&settings, parts, None, fault).expect("daemon boot");
    let addr = daemon.floor_addr().expect("Floor address");
    Boot {
        running: common::start(daemon),
        host,
        addr,
        settings,
    }
}

fn end_turn(boot: &Boot, fixture: &Fixture, turn: TurnId, final_reply: Option<&str>) {
    common::end_turn_with_native_payload(
        fixture,
        &boot.host,
        Input::ChildExited {
            final_reply: final_reply.map(|text| dudes_proto::ChatReply {
                text: text.to_owned(),
                completeness: dudes_proto::ChatReplyCompleteness::Complete,
            }),
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        },
    );
}

/// The turn the journal issued that is not one of `seen`.
fn next_turn(fixture: &Fixture, goal: GoalId, seen: &[TurnId]) -> TurnId {
    fixture.await_record("a new TurnIssued", |record| match record {
        Record::TurnIssued {
            turn, goal: issued, ..
        } if *issued == goal && !seen.contains(turn) => Some(*turn),
        _ => None,
    })
}

fn done_count(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::LoopDecided {
                    decision: dudes_proto::LoopDecision::Done,
                    ..
                }
            )
        })
        .count()
}

fn await_done(fixture: &Fixture, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while done_count(fixture) < count {
        assert!(
            Instant::now() < deadline,
            "no Done decision reached the journal"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn finish_goal(boot: &Boot, fixture: &Fixture, agent: AgentId, seen: &mut Vec<TurnId>) {
    let goal = common::set_goal(
        &boot.running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let turn = next_turn(fixture, goal, seen);
    seen.push(turn);
    end_turn(boot, fixture, turn, Some("the goal is complete"));
}

#[test]
fn the_count_rests_on_an_observed_pass_and_survives_replacement_and_restart() {
    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::ticking(AUCKLAND_NINE_PM, 0));
    let day = Day::new(chrono_tz::Pacific::Auckland);
    // The first predicate fails, so the agent's claim of completion is a self-report with nothing
    // observed behind it; every later one passes.
    let runner = Arc::new(ScriptedRunner::scripted([fails(), passes()]));
    let mut boot = boot_with(&fixture, &clock, &runner, &day);
    let agent = common::hire(&boot.running.handle, fixture.state.path());
    let (cookie, csrf) = session(&boot.settings, boot.addr);
    let (mut browser, snapshot) = Browser::subscribe(boot.addr, &cookie, &csrf);
    assert_eq!(
        snapshot["finished_today"],
        serde_json::json!({
            "state": "ready", "local_date": "2026-10-07", "utc_offset_minutes": 780,
            "total": "0", "through_lsn": snapshot["finished_today"]["through_lsn"],
        })
    );

    let mut seen = Vec::new();
    let goal = common::set_goal(
        &boot.running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let first = next_turn(&fixture, goal, &seen);
    seen.push(first);
    end_turn(
        &boot,
        &fixture,
        first,
        Some("Finished! The goal is complete."),
    );
    let second = next_turn(&fixture, goal, &seen);
    seen.push(second);
    assert_eq!(
        done_count(&fixture),
        0,
        "a self-report with a failing predicate is not Done"
    );
    assert_eq!(runner.runs(), 1);
    let quiet = browser.until("a delta after the failed check", |m| {
        m["type"] == "floor.delta"
    });
    assert!(
        quiet.get("finished_today").is_none(),
        "nothing was finished, so the sibling is not resent: {quiet}"
    );

    let before_done = fixture.entries().len();
    end_turn(&boot, &fixture, second, None);
    await_done(&fixture, 1);
    let done = browser.until("the first completion", |m| total_of(m) == Some("1"));
    assert_eq!(done["finished_today"]["local_date"], "2026-10-07");
    assert!(
        fixture.entries().len() > before_done,
        "positive control: the journal moved for the loop, not for the day"
    );

    finish_goal(&boot, &fixture, agent, &mut seen);
    await_done(&fixture, 2);
    let replaced = browser.until("the second completion", |m| total_of(m) == Some("2"));
    assert_eq!(
        replaced["finished_today"]["total"], "2",
        "a replacement goal counts apart and does not erase the first"
    );

    let entries_before_reads = fixture.entries().len();
    let (mut late, late_snapshot) = Browser::subscribe(boot.addr, &cookie, &csrf);
    assert_eq!(late_snapshot["finished_today"]["total"], "2");
    assert!(late.next(Duration::from_millis(1_500)).is_none());
    assert_eq!(
        fixture.entries().len(),
        entries_before_reads,
        "reading the day appends no journal record"
    );
    drop(browser);
    drop(late);
    assert_eq!(
        boot.running.stop(ExitCause::ShutdownRpc),
        ExitCause::ShutdownRpc
    );

    let restarted = boot_with(&fixture, &clock, &runner, &day);
    let (cookie, csrf) = session(&restarted.settings, restarted.addr);
    let (_fresh, snapshot) = Browser::subscribe(restarted.addr, &cookie, &csrf);
    assert_eq!(
        snapshot["finished_today"]["total"], "2",
        "a fresh browser after a restart recovers the same count"
    );
    drop(restarted);
}
