//! `POST /floor/memory/page` and `POST /floor/memory/entry`, driven through the real Floor gateway
//! against a real daemon whose memory the loop writes.
#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};
use duded::floor_gateway::FloorStartFault;
use duded::floor_memory::SESSION_INTERVAL;
use duded::floor_session::CSRF_HEADER_NAME;
use duded::ports::{Answered, Ask};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_journal::MemoryCondenseConfig;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_proto::{PayloadDigest, PeerProvenance, Record};
use dudes_rpc::Client;
use dudes_types::{AgentId, Lsn};
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);
const PAGE: &str = "/floor/memory/page";
const ENTRY: &str = "/floor/memory/entry";
const WAIT: Duration = Duration::from_secs(20);
/// Just over the routes' per-session interval, so a paced session is never `busy`.
const PACE: Duration = Duration::from_millis(270);
const SENTINEL: &str = "memory-sentinel-7f3a2c";

struct Daemon {
    tokens: dudes_supervisor::testing::FakeTokens,
    running: common::Running,
    settings: duded::Settings,
    addr: SocketAddr,
    clock: Arc<TestClock>,
    pause: duded::runtime::MemoryCondensePause,
    failure: duded::runtime::AppendFailure,
}

impl Daemon {
    fn boot(fixture: &Fixture, memory: MemoryCondenseConfig) -> Self {
        Self::boot_with(
            fixture,
            memory,
            ScriptedColumn::new(PeerProvenance::NoRecordedAgent),
        )
    }

    fn boot_with(
        fixture: &Fixture,
        memory: MemoryCondenseConfig,
        column: Arc<ScriptedColumn>,
    ) -> Self {
        Self::boot_with_interval(fixture, memory, column, SESSION_INTERVAL)
    }

    fn boot_with_interval(
        fixture: &Fixture,
        memory: MemoryCondenseConfig,
        column: Arc<ScriptedColumn>,
        interval: Duration,
    ) -> Self {
        let mut settings = fixture.settings();
        settings.floor = Some(FloorSettings {
            assets_dir: PathBuf::from(FLOOR_FIXTURE),
            port: Some(0),
        });
        let clock = Arc::new(TestClock::new());
        let seams = common::seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
            column,
        );
        let tokens = seams.rng.tokens();
        let daemon = duded::boot::boot_with_memory_config_and_floor_fault(
            &settings,
            seams,
            memory,
            FloorStartFault::memory_interval(interval),
        )
        .expect("daemon boot");
        let addr = daemon.floor_addr().expect("the Floor listener");
        let pause = daemon.memory_condense_pause();
        let failure = daemon.append_failure();
        Self {
            tokens,
            running: common::start(daemon),
            settings,
            addr,
            clock,
            pause,
            failure,
        }
    }

    fn hire(&self, fixture: &Fixture, name: &str) -> AgentId {
        let cwd = fixture.state.path().join(name);
        std::fs::create_dir(&cwd).expect("a working directory");
        common::hire(&self.running.handle, &cwd)
    }

    fn append(&self, agent: AgentId, text: &str) -> Lsn {
        match self
            .running
            .handle
            .ask(Ask::MemoryAppend {
                agent,
                payload: text.as_bytes().to_vec(),
                provenance: PeerProvenance::Agent(agent),
            })
            .expect("memory append")
        {
            Answered::MemoryAppended(lsn) => lsn,
            other => panic!("memory append answered {other:?}"),
        }
    }

    fn withhold(&self, agent: AgentId, len: u64) {
        self.running
            .handle
            .ask(Ask::MemoryAppendQuarantined {
                agent,
                payload: PayloadDigest {
                    decoded_len: len,
                    blake3: "0".repeat(64),
                },
                provenance: PeerProvenance::Agent(agent),
                reason: "memory text exceeds the append ceiling".to_owned(),
            })
            .expect("memory quarantine");
    }

    fn page(&self) -> Browser {
        let (cookie, csrf) = common::floor_session(&self.settings, self.addr);
        Browser {
            addr: self.addr,
            cookie,
            csrf,
            last: None,
        }
    }

    fn stop(mut self) {
        assert_eq!(
            self.running.stop(duded::ExitCause::Ended),
            duded::ExitCause::Ended
        );
    }
}

/// No condense: tests that are not about the summary keep every entry raw.
fn raw_only() -> MemoryCondenseConfig {
    MemoryCondenseConfig::new(1_024, 128).expect("the largest bounds")
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        assert_eq!(self.status, 200, "{}", self.head);
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("JSON body: {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("a UTF-8 body")
    }
}

fn exchange(addr: SocketAddr, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    stream.write_all(request).expect("request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status");
    Reply {
        status,
        head,
        body: response[split + 4..].to_vec(),
    }
}

/// One request with exactly `headers` after the request line and `Content-Length`.
fn send(addr: SocketAddr, line: &str, headers: &[(&str, String)], body: &str) -> Reply {
    let mut request = format!("{line}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        request.push_str(&[name, ": ", value.as_str(), "\r\n"].concat());
    }
    request.push_str("\r\n");
    request.push_str(body);
    exchange(addr, request.as_bytes())
}

/// A signed-in page that waits out the per-session interval between its own requests.
struct Browser {
    addr: SocketAddr,
    cookie: String,
    csrf: String,
    last: Option<Instant>,
}

impl Browser {
    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Host", self.addr.to_string()),
            ("Origin", format!("http://{}", self.addr)),
            ("Content-Type", "application/json".to_owned()),
            ("Cookie", format!("dudes_floor_session={}", self.cookie)),
            (CSRF_HEADER_NAME, self.csrf.clone()),
        ]
    }

    /// Send at once. The next paced request waits from this reply, because the daemon starts the
    /// interval when the request reaches it, some time after it was sent.
    fn post_now(&mut self, path: &str, body: &str) -> Reply {
        let reply = send(
            self.addr,
            &format!("POST {path} HTTP/1.1"),
            &self.headers(),
            body,
        );
        self.last = Some(Instant::now());
        reply
    }

    fn post(&mut self, path: &str, body: &str) -> Reply {
        if let Some(last) = self.last {
            std::thread::sleep(PACE.saturating_sub(last.elapsed()));
        }
        self.post_now(path, body)
    }

    fn page_reply(&mut self, agent: AgentId, before: Option<&str>) -> Reply {
        self.post(
            PAGE,
            &json!({ "agent_id": agent.to_string(), "before": before }).to_string(),
        )
    }

    fn page(&mut self, agent: AgentId, before: Option<&str>) -> Value {
        self.page_reply(agent, before).json()
    }

    fn entry(&mut self, agent: AgentId, lsn: Lsn) -> Value {
        self.post(
            ENTRY,
            &json!({ "agent_id": agent.to_string(), "lsn": lsn.0.to_string() }).to_string(),
        )
        .json()
    }
}

fn lsns(page: &Value) -> Vec<String> {
    page["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["lsn"].as_str().expect("an lsn").to_owned())
        .collect()
}

fn text(lsn: Lsn) -> String {
    lsn.0.to_string()
}

/// The raw bytes of one top-level member, between `"name":` and the member that follows it.
fn section<'a>(body: &'a str, name: &str, next: &str) -> &'a str {
    let start = body.find(&format!("\"{name}\":")).expect("the member") + name.len() + 3;
    let end = body
        .rfind(&format!(",\"{next}\":"))
        .expect("the next member");
    &body[start..end]
}

fn credential(settings: &duded::Settings) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential"),
    )
    .expect("a well-formed credential")
}

fn socket(
    settings: &duded::Settings,
) -> Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream> {
    let stream = std::os::unix::net::UnixStream::connect(settings.socket()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    Client::new(
        std::io::BufReader::new(stream.try_clone().expect("clone")),
        stream,
    )
}

fn token_after_mint_pump(
    fixture: &Fixture,
    tokens: &dudes_supervisor::testing::FakeTokens,
    handle: &duded::Handle,
    agent: AgentId,
) -> dudes_proto::rpc::hook::HookToken {
    let token_hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    let token = tokens.token_of(&token_hash);
    let minted = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::HookTokenMinted {
                agent: owner,
                token_hash: hash,
            } if owner == agent && hash == token_hash => Some(lsn),
            _ => None,
        })
        .expect("the exact agent's durable mint");
    assert!(
        handle
            .doctor()
            .expect("read after the mint's setup pump")
            .lsn
            >= minted
    );
    token
}

/// The agent appends through its own token; after the hire the operator's own
/// `memory.append --agent` and the agent's over-ceiling append are both withheld.
#[test]
fn an_operator_reads_an_agents_entry_and_its_withheld_appends() {
    const APPLIED: &str = "APPLIED-through-the-agent-token";
    let fixture = Fixture::new();
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let daemon = Daemon::boot_with(&fixture, raw_only(), Arc::clone(&column));
    let cwd = fixture.state.path().join("agent");
    std::fs::create_dir(&cwd).expect("a working directory");
    let hired = socket(&daemon.settings)
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(&cwd)).expect("hire params")),
            credential(&daemon.settings),
        )
        .expect("agent.hire");
    let agent: AgentId =
        serde_json::from_value(hired.result.expect("hired")["agent"].clone()).expect("an agent id");
    common::set_goal(
        &daemon.running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    fixture.await_turn();
    let token = token_after_mint_pump(&fixture, &daemon.tokens, &daemon.running.handle, agent);

    column.set(PeerProvenance::Agent(agent));
    let applied = socket(&daemon.settings)
        .call(
            Method::MemoryAppend,
            Some(json!({ "token": token.expose(), "text": APPLIED })),
        )
        .expect("the agent's append");
    assert!(applied.error.is_none(), "{applied:?}");

    column.set(PeerProvenance::NoRecordedAgent);
    let operator = socket(&daemon.settings)
        .call_as_operator(
            Method::MemoryAppend,
            Some(json!({ "agent": agent, "text": "from the operator" })),
            credential(&daemon.settings),
        )
        .expect("the operator's append");
    assert_eq!(
        operator.error.expect("withheld after the hire").code,
        ErrorCode::Refused.code()
    );

    column.set(PeerProvenance::Agent(agent));
    let over = socket(&daemon.settings)
        .call(
            Method::MemoryAppend,
            Some(json!({ "token": token.expose(), "text": "x".repeat(65_537) })),
        )
        .expect("the agent's over-ceiling append");
    assert!(over.error.is_some(), "an over-ceiling append is withheld");
    column.set(PeerProvenance::NoRecordedAgent);

    let positions: Vec<(Lsn, i64)> = fixture
        .entries()
        .into_iter()
        .filter(|(_, entry)| {
            matches!(
                entry.record,
                Record::MemoryAppend { .. } | Record::MemoryAppendQuarantined { .. }
            )
        })
        .map(|(lsn, entry)| (lsn, entry.at.unix_millis()))
        .collect();
    let [
        (applied_lsn, applied_at),
        (operator_lsn, operator_at),
        (over_lsn, over_at),
    ] = positions[..]
    else {
        panic!("three memory records: {positions:?}")
    };

    let page = daemon.page().page(agent, None);
    assert_eq!(
        page,
        json!({
            "state": "ok", "retired": false, "fenced": false, "count": 1, "through": null,
            "summary": null,
            "withheld": { "count": 2, "newest": [
                { "lsn": text(over_lsn), "at": over_at, "writer": { "kind": "agent", "agent": agent },
                  "len": 65_537, "reason": "over_ceiling" },
                { "lsn": text(operator_lsn), "at": operator_at, "writer": { "kind": "outside_agents" },
                  "len": 17, "reason": "authority_unavailable" },
            ]},
            "entries": [
                { "lsn": text(applied_lsn), "at": applied_at, "writer": "agent", "condensed": false,
                  "len": APPLIED.len(), "text": APPLIED, "cut": false },
            ],
            "older": null,
        })
    );
    daemon.stop();
}

/// Every authentication failure is an empty `403`, before any read; near-miss paths
/// and methods are not routed.
#[test]
fn authentication_fails_closed_before_any_read_and_near_misses_are_not_routes() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, SENTINEL);
    let mut page = daemon.page();
    let body = json!({ "agent_id": agent.to_string(), "before": null }).to_string();
    let good = page.headers();
    let without = |name: &str| -> Vec<(&'static str, String)> {
        good.iter()
            .filter(|(header, _)| *header != name)
            .cloned()
            .collect()
    };
    let replaced = |name: &'static str, value: &str| -> Vec<(&'static str, String)> {
        good.iter()
            .map(|(header, old)| {
                (
                    *header,
                    if *header == name {
                        value.to_owned()
                    } else {
                        old.clone()
                    },
                )
            })
            .collect()
    };
    let line = format!("POST {PAGE} HTTP/1.1");
    for (what, headers) in [
        ("no cookie", without("Cookie")),
        (
            "a forged cookie",
            replaced("Cookie", "dudes_floor_session=forged"),
        ),
        ("a wrong Host", replaced("Host", "127.0.0.1:1")),
        ("a wrong Origin", replaced("Origin", "http://evil.example")),
        ("no CSRF", without(CSRF_HEADER_NAME)),
        ("a wrong CSRF", replaced(CSRF_HEADER_NAME, &"A".repeat(43))),
    ] {
        let reply = send(daemon.addr, &line, &headers, &body);
        assert_eq!(reply.status, 403, "{what}: {}", reply.head);
        assert!(reply.body.is_empty(), "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }

    for (what, line) in [
        ("a trailing slash", format!("POST {PAGE}/ HTTP/1.1")),
        ("a query string", format!("POST {PAGE}?before=1 HTTP/1.1")),
        ("GET", format!("GET {PAGE} HTTP/1.1")),
    ] {
        let reply = send(daemon.addr, &line, &good, &body);
        assert_ne!(reply.status, 200, "{what}: {}", reply.head);
        assert!(!reply.head.contains("application/json"), "{what}");
        assert!(!reply.text().contains(SENTINEL), "{what}");
    }

    assert!(
        page.page(agent, None).to_string().contains(SENTINEL),
        "positive control: the signed-in page reads the entry"
    );
    daemon.clock.advance(31 * 60_000);
    let expired = page.page_reply(agent, None);
    assert_eq!(expired.status, 403, "an expired session: {}", expired.head);
    assert!(expired.body.is_empty());
    daemon.stop();
}

/// Bodies over the cap are refused before they are read; malformed ones are `400`.
#[test]
fn bodies_are_bounded_and_strictly_decoded() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let mut page = daemon.page();
    let id = agent.to_string();
    for path in [PAGE, ENTRY] {
        let mut request = format!("POST {path} HTTP/1.1\r\nContent-Length: 129\r\n");
        for (name, value) in page.headers() {
            request.push_str(&[name, ": ", value.as_str(), "\r\n"].concat());
        }
        request.push_str("\r\n");
        let over = exchange(daemon.addr, request.as_bytes());
        assert_eq!(over.status, 413, "{path}: {}", over.head);
    }
    for (path, bad) in [
        (
            PAGE,
            json!({ "agent_id": id, "before": null, "extra": 1 }).to_string(),
        ),
        (PAGE, json!({ "agent_id": id }).to_string()),
        (PAGE, json!([id, null]).to_string()),
        (
            PAGE,
            json!({ "agent_id": "not-an-agent", "before": null }).to_string(),
        ),
        (PAGE, json!({ "agent_id": id, "before": "01" }).to_string()),
        (PAGE, json!({ "agent_id": id, "before": "-1" }).to_string()),
        (
            PAGE,
            json!({ "agent_id": id, "before": "18446744073709551616" }).to_string(),
        ),
        (ENTRY, json!({ "agent_id": id, "lsn": "01" }).to_string()),
        (ENTRY, json!({ "agent_id": id, "lsn": null }).to_string()),
        (
            ENTRY,
            json!({ "agent_id": id, "lsn": "1", "before": null }).to_string(),
        ),
    ] {
        let reply = page.post(path, &bad);
        assert_eq!(reply.status, 400, "{path} {bad}: {}", reply.head);
    }
    let edge = page.page(agent, Some("18446744073709551615"));
    assert_eq!((edge["state"].as_str(), lsns(&edge).len()), (Some("ok"), 0));
    daemon.stop();
}

/// The count bound.
#[test]
fn a_first_page_holds_thirty_two_entries() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let written: Vec<Lsn> = (0..40)
        .map(|n| daemon.append(agent, &format!("note {n}")))
        .collect();
    let first = daemon.page().page(agent, None);
    let expected: Vec<String> = written.iter().rev().take(32).copied().map(text).collect();
    assert_eq!(lsns(&first), expected);
    assert_eq!(first["older"], json!(text(written[8])));
    assert_eq!(first["count"], 40);
    daemon.stop();
}

/// The byte bounds at the JSON escaping worst case.
#[test]
fn pages_and_entries_stay_within_their_byte_bounds_at_six_to_one_escaping() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let worst = "\u{1}".repeat(2_048);
    let written: Vec<Lsn> = (0..12).map(|_| daemon.append(agent, &worst)).collect();
    let mut page = daemon.page();

    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let reply = page.page_reply(agent, before.as_deref());
        let body = reply.text();
        assert!(body.len() <= 131_072, "page {} bytes", body.len());
        let entries = section(&body, "entries", "older");
        assert!(entries.len() <= 65_536, "entries {} bytes", entries.len());
        let value = reply.json();
        let kept = lsns(&value);
        assert!(!kept.is_empty() && kept.len() < 12, "{kept:?}");
        for entry in value["entries"].as_array().expect("entries") {
            assert_eq!(entry["text"], json!(worst), "a whole preview");
        }
        seen.extend(kept.iter().cloned());
        if value["older"].is_null() {
            break;
        }
        assert_eq!(
            value["older"].as_str(),
            kept.last().map(String::as_str),
            "the byte-cut page's older names its last kept entry"
        );
        before = value["older"].as_str().map(str::to_owned);
    }
    let expected: Vec<String> = written.iter().rev().copied().map(text).collect();
    assert_eq!(seen, expected, "no entry skipped and none repeated");

    let large = daemon.append(agent, &"\u{1}".repeat(65_536));
    let whole = page.post(
        ENTRY,
        &json!({ "agent_id": agent.to_string(), "lsn": text(large) }).to_string(),
    );
    assert!(
        whole.body.len() <= 409_600,
        "entry {} bytes",
        whole.body.len()
    );
    let whole = whole.json();
    assert_eq!(whole["entry"]["len"], 65_536);
    assert_eq!(whole["entry"]["cut"], false);
    assert_eq!(whole["entry"]["text"].as_str().map(str::len), Some(65_536));

    let crab = format!("{}🦀after", "a".repeat(2_046));
    let cut = daemon.append(agent, &crab);
    let first = page.page(agent, None);
    let preview = &first["entries"][0];
    assert_eq!(preview["lsn"], json!(text(cut)));
    assert_eq!(preview["text"], json!("a".repeat(2_046)));
    assert_eq!(preview["cut"], true);
    assert_eq!(preview["len"], 2_055);
    daemon.stop();
}

/// The summary bound, with full entries after the summary.
#[test]
fn a_worst_case_summary_and_full_entries_fit_one_page() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(
        &fixture,
        MemoryCondenseConfig::new(1, 8).expect("small bounds"),
    );
    let agent = daemon.hire(&fixture, "agent");
    let worst = "\u{1}".repeat(2_048);
    for _ in 0..40 {
        daemon.append(agent, &worst);
    }
    let reply = daemon.page().page_reply(agent, None);
    let body = reply.text();
    assert!(body.len() <= 131_072, "page {} bytes", body.len());
    let summary = section(&body, "summary", "withheld");
    assert!(summary.len() <= 51_200, "summary {} bytes", summary.len());
    let value = reply.json();
    assert_eq!(value["summary"]["kind"], "excerpts");
    assert!(
        value["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["condensed"] == false && entry["text"] == json!(worst)),
        "full entries follow the summary"
    );
    daemon.stop();
}

/// A walk from the newest entry visits each once, and appends between pages leave
/// the older pages as they were.
#[test]
fn a_walk_visits_every_entry_once_while_appends_continue() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let written: Vec<Lsn> = (0..100)
        .map(|n| daemon.append(agent, &format!("entry {n}")))
        .collect();
    let mut page = daemon.page();
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let value = page.page(agent, before.as_deref());
        seen.extend(lsns(&value));
        daemon.append(agent, "appended between pages");
        match value["older"].as_str() {
            Some(older) => before = Some(older.to_owned()),
            None => break,
        }
    }
    let expected: Vec<String> = written.iter().rev().copied().map(text).collect();
    assert_eq!(seen, expected);
    daemon.stop();
}

/// An exact-excerpt condense: its summary, its coverage, and the entries it covers.
#[test]
fn a_condense_shows_its_summary_and_marks_the_entries_it_covers() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(
        &fixture,
        MemoryCondenseConfig::new(1, 4).expect("small bounds"),
    );
    let agent = daemon.hire(&fixture, "agent");
    let condensed_to = |fixture: &Fixture| -> Vec<Lsn> {
        fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::MemoryCondense { to_lsn, .. } => Some(to_lsn),
                _ => None,
            })
            .collect()
    };
    let mut written = Vec::new();
    let append_until_condensed = |fixture: &Fixture, written: &mut Vec<Lsn>| {
        let before = condensed_to(fixture).len();
        for _ in 0..32 {
            let n = written.len();
            written.push(daemon.append(agent, &format!("decision {n}: {}", "d".repeat(2_000))));
            if condensed_to(fixture).len() > before {
                return;
            }
        }
        panic!("no condense after 32 appends");
    };
    append_until_condensed(&fixture, &mut written);
    let through = *condensed_to(&fixture).last().expect("a condense");
    let mut page = daemon.page();
    let first = page.page(agent, None);
    assert_eq!(first["summary"]["kind"], "excerpts");
    assert_eq!(first["through"], json!(text(through)));
    for entry in first["entries"].as_array().expect("entries") {
        let lsn: u64 = entry["lsn"]
            .as_str()
            .expect("lsn")
            .parse()
            .expect("decimal");
        assert_eq!(entry["condensed"], json!(lsn <= through.0), "{entry}");
    }
    assert!(
        first["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["condensed"] == true),
        "positive control: a covered entry is on the first page"
    );

    let later = page.page(agent, Some(&text(written[written.len() - 1])));
    assert_eq!(later["summary"], Value::Null);
    assert_eq!(later["withheld"], Value::Null);
    assert_eq!(later["through"], json!(text(through)));

    append_until_condensed(&fixture, &mut written);
    let after = page.page(agent, Some(&text(written[written.len() - 1])));
    assert_eq!(
        after["through"],
        json!(text(
            *condensed_to(&fixture).last().expect("a second condense")
        ))
    );
    assert_ne!(after["through"], later["through"]);
    daemon.stop();
}

/// The wire carries the text exactly; the browser draws it inert.
#[test]
fn hostile_and_hidden_characters_cross_the_wire_exactly() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let hostile = "\u{1b}[31m red \u{0} nul \u{202e}evil</script><img src=x onerror=alert(1)>";
    let lsn = daemon.append(agent, hostile);
    let mut page = daemon.page();
    let first = page.page(agent, None);
    assert_eq!(first["entries"][0]["text"], json!(hostile), "{first}");
    assert_eq!(page.entry(agent, lsn)["entry"]["text"], json!(hostile));
    daemon.stop();
}

/// A retired agent's memory still reads, from its retirement on.
#[test]
fn a_retired_agents_memory_reads_and_an_unknown_agent_is_a_state() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let written: Vec<Lsn> = (0..40)
        .map(|n| daemon.append(agent, &format!("kept {n}")))
        .collect();
    let mut page = daemon.page();
    let live = page.page(agent, None);
    assert_eq!(live["retired"], false);
    daemon
        .running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "done".to_owned(),
            },
        )))
        .expect("retire");
    let older = page.page(agent, live["older"].as_str());
    assert_eq!(older["state"], "ok");
    assert_eq!(older["retired"], true, "retired between two pages");
    assert_eq!(
        lsns(&older),
        written[..8]
            .iter()
            .rev()
            .copied()
            .map(text)
            .collect::<Vec<_>>()
    );
    let again = page.page(agent, None);
    assert_eq!(again["retired"], true);
    assert_eq!(lsns(&again), lsns(&live));

    let stranger = AgentId::parse("01J00000000000000000000099").expect("an id");
    let unknown = page.page(stranger, None);
    assert_eq!(
        unknown,
        json!({ "state": "agent_unknown", "retired": false, "fenced": false, "count": 0,
                "through": null, "summary": null, "withheld": null, "entries": [], "older": null })
    );
    assert_eq!(
        page.entry(stranger, written[0]),
        json!({ "state": "agent_unknown", "entry": null })
    );
    daemon.stop();
}

/// One entry by position, and nothing at a position that is not this agent's entry.
#[test]
fn an_entry_reads_whole_at_its_own_position_only() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let other = daemon.hire(&fixture, "other");
    let long = format!("{}TAIL", "w".repeat(5_000));
    let own = daemon.append(agent, &long);
    let theirs = daemon.append(other, "theirs");
    daemon.withhold(agent, 70_000);
    let withheld = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record, Record::MemoryAppendQuarantined { .. }).then_some(lsn)
        })
        .expect("a withheld append");
    let mut page = daemon.page();
    let whole = page.entry(agent, own);
    assert_eq!(whole["state"], "ok");
    assert_eq!(whole["entry"]["text"], json!(long));
    assert_eq!(whole["entry"]["cut"], false);
    assert_eq!(whole["entry"]["len"], 5_004);
    for lsn in [theirs, withheld] {
        assert_eq!(
            page.entry(agent, lsn),
            json!({ "state": "not_found", "entry": null })
        );
    }
    daemon.stop();
}

/// A flood of withheld appends cannot push entries off the first page.
#[test]
fn a_withheld_flood_leaves_the_entries_on_the_first_page() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let written: Vec<Lsn> = (0..10)
        .map(|n| daemon.append(agent, &format!("real {n}")))
        .collect();
    for _ in 0..50 {
        daemon.withhold(agent, 65_537);
    }
    let first = daemon.page().page(agent, None);
    assert_eq!(
        lsns(&first),
        written.iter().rev().copied().map(text).collect::<Vec<_>>()
    );
    assert_eq!(first["withheld"]["count"], 50);
    let markers = first["withheld"]["newest"].as_array().expect("markers");
    assert_eq!(markers.len(), 8);
    assert!(
        markers
            .iter()
            .all(|marker| marker["reason"] == "over_ceiling"
                && marker["len"] == 65_537
                && marker.get("text").is_none())
    );
    assert_eq!(first["older"], Value::Null);
    daemon.stop();
}

/// An integrity fence is reported.
#[test]
fn a_fenced_daemon_says_so_on_the_page() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(
        &fixture,
        MemoryCondenseConfig::new(1, 1).expect("small bounds"),
    );
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, &"a".repeat(2_048));
    let mut page = daemon.page();
    assert_eq!(page.page(agent, None)["fenced"], false);
    daemon.pause.arm();
    let handle = daemon.running.handle.clone();
    let appending = std::thread::spawn(move || {
        handle.ask(Ask::MemoryAppend {
            agent,
            payload: "b".repeat(2_048).into_bytes(),
            provenance: PeerProvenance::Agent(agent),
        })
    });
    assert!(daemon.pause.wait_reached(WAIT), "the condense paused");
    let summary = fixture
        .records()
        .iter()
        .find_map(|record| match record {
            Record::MemoryCondense { summary_ref, .. } => {
                Some(fixture.state.path().join(summary_ref))
            }
            _ => None,
        })
        .expect("a summary");
    let mut bytes = std::fs::read(&summary).expect("summary bytes");
    bytes[0] ^= 1;
    std::fs::write(&summary, &bytes).expect("tamper");
    daemon.pause.release();
    assert!(appending.join().expect("joined").is_err());
    let fenced = page.page(agent, None);
    assert_eq!(fenced["state"], "ok");
    assert_eq!(fenced["fenced"], true);
    daemon.stop();
}

/// A session that expires while its read waits for the loop gets the empty `403`, not the page:
/// validity is checked again after the loop answers.
#[test]
fn a_session_that_expires_while_the_loop_answers_gets_no_page() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(
        &fixture,
        MemoryCondenseConfig::new(1, 1).expect("small bounds"),
    );
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, &format!("{SENTINEL}{}", "a".repeat(2_048)));
    let page = daemon.page();
    daemon.pause.arm();
    let handle = daemon.running.handle.clone();
    let appending = std::thread::spawn(move || {
        handle.ask(Ask::MemoryAppend {
            agent,
            payload: "b".repeat(2_048).into_bytes(),
            provenance: PeerProvenance::Agent(agent),
        })
    });
    assert!(
        daemon.pause.wait_reached(WAIT),
        "the loop is held inside a condense"
    );

    // Two reads from one session: one is admitted and waits on the held loop; the other finds the
    // session's interval taken and answers at once, which proves the first was admitted.
    let body = json!({ "agent_id": agent.to_string(), "before": null }).to_string();
    let (replies, answered) = mpsc::channel();
    for _ in 0..2 {
        let (replies, headers, body, addr) =
            (replies.clone(), page.headers(), body.clone(), daemon.addr);
        std::thread::spawn(move || {
            let _ = replies.send(send(
                addr,
                &format!("POST {PAGE} HTTP/1.1"),
                &headers,
                &body,
            ));
        });
    }
    let busy = answered
        .recv_timeout(WAIT)
        .expect("one read answers while the loop is held");
    assert_eq!(busy.json()["state"], "busy");

    daemon.clock.advance(31 * 60_000);
    daemon.pause.release();
    assert!(appending.join().expect("the append").is_ok());
    let held = answered.recv_timeout(WAIT).expect("the held read answers");
    assert_eq!(held.status, 403, "{}", held.head);
    assert!(held.body.is_empty(), "{}", held.text());
    daemon.stop();
}

/// A stopped journal does not stop a read, which appends nothing.
#[test]
fn a_stopped_journal_still_answers_the_page() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, SENTINEL);
    daemon.failure.short_write_next(5);
    assert!(
        daemon
            .running
            .handle
            .ask(Ask::MemoryAppend {
                agent,
                payload: b"lost".to_vec(),
                provenance: PeerProvenance::Agent(agent),
            })
            .is_err(),
        "the journal stopped"
    );
    let page = daemon.page().page(agent, None);
    assert_eq!(page["state"], "ok");
    assert_eq!(page["entries"][0]["text"], SENTINEL);
    daemon.stop();
}

/// One session cannot start a second read inside its interval; two sessions do not wait for each
/// other. The interval is far longer than any pause the test can suffer, so this cannot race.
#[test]
fn a_session_reading_again_within_its_interval_is_busy() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot_with_interval(
        &fixture,
        raw_only(),
        ScriptedColumn::new(PeerProvenance::NoRecordedAgent),
        Duration::from_secs(600),
    );
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, "one");
    let body = json!({ "agent_id": agent.to_string(), "before": null }).to_string();
    let mut first = daemon.page();
    let mut second = daemon.page();
    assert_eq!(first.post_now(PAGE, &body).json()["state"], "ok");
    assert_eq!(second.post_now(PAGE, &body).json()["state"], "ok");
    let again = first.post_now(PAGE, &body).json();
    assert_eq!(
        again,
        json!({ "state": "busy", "retired": false, "fenced": false, "count": 0, "through": null,
                "summary": null, "withheld": null, "entries": [], "older": null })
    );
    let entry = first
        .post_now(
            ENTRY,
            &json!({ "agent_id": agent.to_string(), "lsn": "1" }).to_string(),
        )
        .json();
    assert_eq!(entry, json!({ "state": "busy", "entry": null }));
    daemon.stop();
}

/// A session reads again once its interval has passed. Sleeping past the interval is a lower
/// bound, so a slow runner cannot make this fail.
#[test]
fn a_session_reading_after_its_interval_is_answered() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    daemon.append(agent, "one");
    let body = json!({ "agent_id": agent.to_string(), "before": null }).to_string();
    let mut page = daemon.page();
    assert_eq!(page.post_now(PAGE, &body).json()["state"], "ok");
    std::thread::sleep(PACE);
    assert_eq!(page.post_now(PAGE, &body).json()["state"], "ok");
    daemon.stop();
}

/// A restart replays the same entries, times and writers.
#[test]
fn a_restart_returns_the_same_pages() {
    let fixture = Fixture::new();
    let daemon = Daemon::boot(&fixture, raw_only());
    let agent = daemon.hire(&fixture, "agent");
    let mut last = Lsn::ZERO;
    for n in 0..40 {
        last = daemon.append(agent, &format!("before the restart {n}"));
    }
    daemon.withhold(agent, 70_000);
    let mut page = daemon.page();
    let first = page.page(agent, None);
    let older = page.page(agent, first["older"].as_str());
    let entry = page.entry(agent, last);
    daemon.stop();

    let restarted = Daemon::boot(&fixture, raw_only());
    let mut page = restarted.page();
    assert_eq!(page.page(agent, None), first);
    assert_eq!(page.page(agent, first["older"].as_str()), older);
    assert_eq!(page.entry(agent, last), entry);
    restarted.stop();
}

/// The day source the stream samples, moved by the test to cross a midnight and resync.
struct Day(Arc<std::sync::atomic::AtomicI64>);

impl Day {
    fn source(&self) -> duded::local_day::LocalDays {
        let now = Arc::clone(&self.0);
        duded::local_day::LocalDays::from_fn(move || {
            duded::local_day::window_in(
                &chrono_tz::UTC,
                dudes_types::Timestamp::from_unix_millis(
                    now.load(std::sync::atomic::Ordering::SeqCst),
                ),
            )
        })
    }
}

/// A subscribed stream on one session, reading server text frames.
struct Stream(TcpStream);

impl Stream {
    fn subscribe(addr: SocketAddr, cookie: &str, csrf: &str) -> (Self, Value) {
        let mut stream = TcpStream::connect(addr).expect("Floor listener");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        let request = format!(
            "GET /floor/stream HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).expect("upgrade");
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("upgrade head");
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&head)
        );
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
        stream.write_all(&frame).expect("subscribe");
        let mut stream = Self(stream);
        let snapshot = stream.next(WAIT).expect("a snapshot");
        assert_eq!(snapshot["type"], "floor.snapshot");
        (stream, snapshot)
    }

    /// The next server message as raw text, or `None` once `bound` passes with nothing sent.
    fn next_text(&mut self, bound: Duration) -> Option<String> {
        self.0.set_read_timeout(Some(bound)).expect("timeout");
        let mut head = [0_u8; 2];
        match self.0.read_exact(&mut head) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return None;
            }
            Err(error) => panic!("server frame: {error}"),
        }
        let length = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0_u8; 2];
                self.0.read_exact(&mut bytes).expect("u16 length");
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0_u8; 8];
                self.0.read_exact(&mut bytes).expect("u64 length");
                usize::try_from(u64::from_be_bytes(bytes)).expect("fits")
            }
        };
        self.0.set_read_timeout(Some(WAIT)).expect("timeout");
        let mut body = vec![0_u8; length];
        self.0.read_exact(&mut body).expect("frame body");
        Some(String::from_utf8(body).expect("a text frame"))
    }

    fn next(&mut self, bound: Duration) -> Option<Value> {
        self.next_text(bound)
            .map(|text| serde_json::from_str(&text).expect("server JSON"))
    }
}

/// A session that read a page holding the sentinel never receives it on its
/// stream: not in its snapshot, not in a delta after a new append, not after a resync.
#[test]
fn a_page_read_never_reaches_the_sessions_stream() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    let day = Day(Arc::new(std::sync::atomic::AtomicI64::new(
        1_787_443_200_000 - 60_000,
    )));
    let fault = duded::floor_stream::FloorStreamFault::default().without_heartbeat();
    fault.use_local_days(day.source());
    let daemon = duded::boot::boot_with_floor_faults(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
        None,
        fault,
    )
    .expect("daemon boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    let mut running = common::start(daemon);
    let cwd = fixture.state.path().join("agent");
    std::fs::create_dir(&cwd).expect("a working directory");
    let agent = common::hire(&running.handle, &cwd);
    let append = |text: &str| {
        running
            .handle
            .ask(Ask::MemoryAppend {
                agent,
                payload: text.as_bytes().to_vec(),
                provenance: PeerProvenance::Agent(agent),
            })
            .expect("memory append");
    };
    append(SENTINEL);

    let (cookie, csrf) = common::floor_session(&settings, addr);
    let mut page = Browser {
        addr,
        cookie: cookie.clone(),
        csrf: csrf.clone(),
        last: None,
    };
    assert!(
        page.page_reply(agent, None).text().contains(SENTINEL),
        "positive control: the page holds the sentinel"
    );

    let (mut stream, snapshot) = Stream::subscribe(addr, &cookie, &csrf);
    let mut frames = vec![snapshot.to_string()];
    append(&format!("{SENTINEL}-after"));
    running
        .handle
        .ask(Ask::Pause(agent))
        .expect("a pause the stream reports");
    let delta = loop {
        let text = stream.next_text(WAIT).expect("a delta after the append");
        frames.push(text.clone());
        let value: Value = serde_json::from_str(&text).expect("server JSON");
        if value["type"] == "floor.delta" && value["delta"]["agents"][0]["paused"] == true {
            break value;
        }
    };
    assert_eq!(delta["delta"]["agents"][0]["id"], json!(agent));

    day.0
        .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
    loop {
        let text = stream.next_text(WAIT).expect("a resync at midnight");
        frames.push(text.clone());
        if text == r#"{"type":"floor.resync"}"# {
            break;
        }
    }
    drop(stream);
    let (_fresh, resynced) = Stream::subscribe(addr, &cookie, &csrf);
    frames.push(resynced.to_string());

    for frame in &frames {
        assert!(
            !frame.contains(SENTINEL),
            "a stream frame held memory: {frame}"
        );
    }
    assert!(frames.len() >= 4, "{frames:?}");
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}
