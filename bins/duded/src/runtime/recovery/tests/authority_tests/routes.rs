#![cfg(test)]

use super::*;
use crate::floor_session::Exchange;
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn post(addr: SocketAddr, cookie: &str, csrf: &str, route: &str, body: &[u8]) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "POST {route} HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    stream.write_all(body).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    let split = reply
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let status = std::str::from_utf8(&reply[..split])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = &reply[split + 4..];
    assert!(body.len() <= 4096);
    (status, serde_json::from_slice(body).unwrap_or(Value::Null))
}

#[test]
fn authenticated_http_inspects_the_existing_checked_opportunity() {
    let fixture = common::Fixture::new();
    let settings = floor_settings(&fixture, 0);
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let agent = failed_source(&mut daemon, &fixture);
    let floor = daemon.floor.as_ref().unwrap();
    let addr = floor.addr();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let exchange = sessions
        .exchange(
            &addr.to_string(),
            &format!("http://{addr}"),
            sessions.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    let before = fixture.entries();
    let mut running = common::start(daemon);
    let (status, response) = post(
        addr,
        &exchange.cookie,
        &exchange.csrf,
        "/floor/recovery/inspect",
        &serde_json::to_vec(&json!({"agent": agent})).unwrap(),
    );
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["state"], "available");
    assert_eq!(response["choices"], "fresh_only");
    assert!(response["expected"]["issue_lsn"].is_string());
    assert_eq!(fixture.entries(), before);
    running.stop(crate::ExitCause::Ended);
}

struct HttpFixture {
    running: common::Running,
    fixture: common::Fixture,
    addr: SocketAddr,
    sessions: FloorSessions,
    exchange: Exchange,
    clock: Arc<common::TestClock>,
    agent: AgentId,
    host: common::SharedHost,
}

impl HttpFixture {
    fn new(recovery: bool, control: bool) -> Self {
        let fixture = common::Fixture::new();
        let mut settings = floor_settings(&fixture, 0);
        settings.floor_recovery_enabled = recovery;
        if !control {
            settings.floor_control = FloorControl::Off;
        }
        let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
        let mut daemon = boot_at(&settings, &clock);
        let agent = failed_source(&mut daemon, &fixture);
        wait_for_human(&mut daemon, agent);
        let floor = daemon.floor.as_ref().unwrap();
        let addr = floor.addr();
        let sessions = floor
            .carry(crate::baseline::CheckRuns::default())
            .sessions()
            .clone();
        let exchange = sessions
            .exchange(
                &addr.to_string(),
                &format!("http://{addr}"),
                sessions.issue().unwrap().bootstrap.as_bytes(),
            )
            .unwrap();
        let host = daemon.ctx.host.clone();
        Self {
            running: common::start(daemon),
            fixture,
            addr,
            sessions,
            exchange,
            clock,
            agent,
            host,
        }
    }

    fn post(&self, route: &str, body: &Value) -> (u16, Value) {
        post(
            self.addr,
            &self.exchange.cookie,
            &self.exchange.csrf,
            route,
            &serde_json::to_vec(body).unwrap(),
        )
    }

    fn inspect(&self) -> Value {
        let (code, view) = self.post("/floor/recovery/inspect", &json!({"agent": self.agent}));
        assert_eq!(code, 200);
        assert_eq!(view["state"], "available");
        assert_eq!(view["waiting"]["open_question"], "waiting");
        view["expected"].clone()
    }

    fn submit(&self, request: &str, expected: &Value) -> (u16, Value) {
        self.post(
            "/floor/recovery/submit",
            &json!({"request_id": request, "expected": expected, "choice": "fresh_with_recap"}),
        )
    }
}

#[test]
fn http_recovery_requires_the_effective_conjunction_but_receipt_reads_remain_available() {
    for (recovery, control) in [(false, false), (false, true), (true, false)] {
        let owned = HttpFixture::new(recovery, control);
        let before = owned.fixture.entries();
        for route in ["/floor/recovery/inspect", "/floor/recovery/submit"] {
            let (status, _) = owned.post(route, &json!({}));
            assert_eq!(status, 400, "disabled POST retains static-route framing");
        }
        let (status, view) = owned.post(
            "/floor/recovery/receipt",
            &json!({"request_id": "74".repeat(32)}),
        );
        assert_eq!(status, 200);
        assert_eq!(view["state"], "absent");
        assert!(view["checked_through"].is_string());
        assert_eq!(owned.fixture.entries(), before);
        assert!(owned.host.spawned().is_empty());
    }
}

#[test]
fn http_inspect_submit_and_inert_receipt_keep_exact_request_and_claim_truth() {
    let owned = HttpFixture::new(true, true);
    let expected = owned.inspect();
    let request = "75".repeat(32);
    let mut stale = expected.clone();
    stale["issue_lsn"] = json!("18446744073709551615");
    let before = owned.fixture.entries();
    let (code, refused) = owned.submit(&request, &stale);
    assert_eq!(code, 409);
    assert_eq!(refused["state"], "refused");
    assert_eq!(refused["reason"], "stale_work");
    assert_eq!(owned.fixture.entries(), before);
    let (code, accepted) = owned.submit(&request, &expected);
    assert_eq!(code, 200, "{accepted}");
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted["receipt"]["request_id"], request);
    assert_eq!(accepted["receipt"]["status"]["state"], "pending");
    assert_eq!(accepted["receipt"]["choice"], "fresh_with_recap");
    assert!(owned.host.spawned().is_empty());
    let after = owned.fixture.entries();
    let (code, duplicate) = owned.submit(&request, &expected);
    assert_eq!(code, 200);
    assert_eq!(duplicate, accepted);
    assert_eq!(owned.fixture.entries(), after);
    assert!(owned.host.spawned().is_empty());
    let (code, receipt) = owned.post("/floor/recovery/receipt", &json!({"request_id": request}));
    assert_eq!(code, 200);
    assert_eq!(
        receipt,
        json!({"state": "found", "receipt": accepted["receipt"]})
    );
    assert_eq!(owned.fixture.entries(), after);
    let (code, refused) = owned.submit(&"76".repeat(32), &expected);
    assert_eq!(code, 409);
    assert_eq!(refused["reason"], "claimed");
    assert_eq!(owned.fixture.entries(), after);
    assert!(owned.host.spawned().is_empty());
    let text = accepted.to_string();
    for forbidden in [
        "session_digest",
        "session_id",
        "original owned task",
        "execution_folder",
        "cwd",
        "transcript",
    ] {
        assert!(!text.contains(forbidden), "{text}");
    }
}

#[test]
fn http_receipts_use_current_readers_without_observing_the_lost_original_authority() {
    let owned = HttpFixture::new(true, true);
    let expected = owned.inspect();
    let request = "77".repeat(32);
    let (code, accepted) = owned.submit(&request, &expected);
    assert_eq!(code, 200);
    assert_eq!(accepted["receipt"]["status"]["state"], "pending");
    let reader = owned
        .sessions
        .exchange(
            &owned.addr.to_string(),
            &format!("http://{}", owned.addr),
            owned.sessions.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    assert!(
        owned
            .sessions
            .authorizer()
            .revoke_for_test(&owned.exchange.cookie)
    );
    let before = owned.fixture.entries();
    for _ in 0..3 {
        let (code, view) = post(
            owned.addr,
            &reader.cookie,
            &reader.csrf,
            "/floor/recovery/receipt",
            &serde_json::to_vec(&json!({"request_id": request})).unwrap(),
        );
        assert_eq!(code, 200);
        assert_eq!(view["state"], "found");
        assert_eq!(view["receipt"], accepted["receipt"]);
    }
    assert_eq!(owned.fixture.entries(), before);
    assert!(owned.host.spawned().is_empty());
    let (code, _) = owned.post("/floor/recovery/receipt", &json!({"request_id": request}));
    assert_eq!(code, 403);
    assert_eq!(owned.fixture.entries(), before);
}

#[test]
fn repeated_http_receipt_reads_cannot_extend_the_registry_idle_deadline() {
    let owned = HttpFixture::new(true, true);
    let request = json!({"request_id": "78".repeat(32)});
    owned.sessions.issue().unwrap();
    assert_eq!(owned.sessions.registry_counts_for_test(), (1, 1));
    let before = owned.fixture.entries();
    owned.clock.advance((24 * 60 - 1) * 60_000);
    let (code, absent) = owned.post("/floor/recovery/receipt", &request);
    assert_eq!(code, 200);
    assert_eq!(absent["state"], "absent");
    assert_eq!(owned.sessions.registry_counts_for_test(), (1, 1));
    owned.clock.advance(59_999);
    assert_eq!(owned.post("/floor/recovery/receipt", &request).0, 200);
    assert_eq!(owned.sessions.registry_counts_for_test(), (1, 1));
    owned.clock.advance(1);
    assert_eq!(owned.post("/floor/recovery/receipt", &request).0, 403);
    assert_eq!(owned.sessions.registry_counts_for_test(), (1, 1));
    assert_eq!(owned.fixture.entries(), before);
    assert!(owned.host.spawned().is_empty());
}

#[test]
fn http_recovery_decodes_original_bodies_and_refuses_hidden_authority_fields() {
    let owned = HttpFixture::new(true, true);
    let expected = owned.inspect();
    let intent =
        json!({"request_id": "79".repeat(32), "expected": expected, "choice": "fresh_with_recap"});
    let mut numeric = intent.clone();
    numeric["expected"]["issue_lsn"] = json!(9_007_199_254_740_993_u64);
    let mut attributed = intent.clone();
    attributed["attribution"] = json!({"kind": "local_operator", "pid": 1});
    let mut task = intent.clone();
    task["task"] = json!("caller supplied");
    let before = owned.fixture.entries();
    for malformed in [numeric, attributed, task, json!([intent])] {
        assert_eq!(owned.post("/floor/recovery/submit", &malformed).0, 400);
    }
    for (route, body) in [
        (
            "/floor/recovery/inspect",
            format!(
                "{{\"agent\":\"{}\",\"agent\":\"{}\"}}",
                owned.agent, owned.agent
            ),
        ),
        (
            "/floor/recovery/receipt",
            format!(
                "{{\"request_id\":\"{}\",\"request_id\":\"{}\"}}",
                "79".repeat(32),
                "79".repeat(32)
            ),
        ),
        (
            "/floor/recovery/receipt",
            format!(
                "{{\"request_id\":\"{}\",\"choice\":\"fresh_with_recap\"}}",
                "79".repeat(32)
            ),
        ),
    ] {
        assert_eq!(
            post(
                owned.addr,
                &owned.exchange.cookie,
                &owned.exchange.csrf,
                route,
                body.as_bytes()
            )
            .0,
            400
        );
    }
    let mut oversized = serde_json::to_vec(&json!({"agent": owned.agent})).unwrap();
    oversized.resize(4097, b' ');
    assert_eq!(
        post(
            owned.addr,
            &owned.exchange.cookie,
            &owned.exchange.csrf,
            "/floor/recovery/inspect",
            &oversized
        )
        .0,
        413
    );
    assert_eq!(owned.fixture.entries(), before);
    assert!(owned.host.spawned().is_empty());
}

#[test]
fn http_recovery_authentication_refuses_foreign_expired_and_revoked_sessions() {
    let owned = HttpFixture::new(true, true);
    let inspect = serde_json::to_vec(&json!({"agent": owned.agent})).unwrap();
    let receipt = serde_json::to_vec(&json!({"request_id": "80".repeat(32)})).unwrap();
    let foreign = FloorSessions::new(owned.addr, Arc::clone(&owned.clock));
    let foreign_exchange = foreign
        .exchange(
            &owned.addr.to_string(),
            &format!("http://{}", owned.addr),
            foreign.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    let before = owned.fixture.entries();
    for route in [
        "/floor/recovery/inspect",
        "/floor/recovery/submit",
        "/floor/recovery/receipt",
    ] {
        assert_eq!(
            post(
                owned.addr,
                &foreign_exchange.cookie,
                &foreign_exchange.csrf,
                route,
                &inspect
            )
            .0,
            403
        );
        assert_eq!(
            post(
                owned.addr,
                &owned.exchange.cookie,
                &"A".repeat(43),
                route,
                &inspect
            )
            .0,
            403
        );
    }
    let old = crate::floor_session::FloorCarry::new(
        foreign.clone(),
        None,
        crate::baseline::CheckRuns::default(),
    );
    let exported = old.export(Some("before"));
    foreign.invalidate();
    let current = crate::floor_session::FloorCarry::new(
        owned.sessions.clone(),
        None,
        crate::baseline::CheckRuns::default(),
    );
    assert_eq!(current.import(Some("after"), &exported), 1);
    assert_eq!(
        post(
            owned.addr,
            &foreign_exchange.cookie,
            &foreign_exchange.csrf,
            "/floor/recovery/receipt",
            &receipt
        )
        .0,
        200,
        "a carried session keeps working with the secret its page holds"
    );
    assert!(
        owned
            .sessions
            .authorizer()
            .revoke_for_test(&owned.exchange.cookie)
    );
    assert_eq!(
        owned
            .post("/floor/recovery/inspect", &json!({"agent": owned.agent}))
            .0,
        403
    );
    assert_eq!(
        post(
            owned.addr,
            &owned.exchange.cookie,
            &owned.exchange.csrf,
            "/floor/recovery/receipt",
            &receipt
        )
        .0,
        403
    );
    assert_eq!(owned.fixture.entries(), before);
    assert!(owned.host.spawned().is_empty());
}

#[test]
fn serialized_receipt_read_revalidates_current_registry_and_expiry() {
    let owned = HttpFixture::new(true, true);
    let host = owned.addr.to_string();
    let origin = format!("http://{host}");
    let foreign = FloorSessions::new(owned.addr, Arc::clone(&owned.clock));
    let exchange = foreign
        .exchange(
            &host,
            &origin,
            foreign.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    let foreign_reader = foreign
        .authorize_receipt(&host, &origin, &exchange.cookie, &exchange.csrf)
        .unwrap();
    let before = owned.fixture.entries();
    assert_eq!(
        owned
            .running
            .handle
            .floor_recovery_receipt(foreign_reader, Hex32::new([81; 32])),
        Err(crate::ports::FloorRecoveryReadError::Forbidden)
    );
    assert_eq!(
        post(
            owned.addr,
            &owned.exchange.cookie,
            &owned.exchange.csrf,
            "/floor/recovery/receipt",
            &serde_json::to_vec(&json!({"request_id": "81".repeat(32)})).unwrap()
        )
        .0,
        200
    );
    let reader = owned
        .sessions
        .authorize_receipt(&host, &origin, &owned.exchange.cookie, &owned.exchange.csrf)
        .unwrap();
    owned.clock.advance(24 * 60 * 60_000);
    assert_eq!(
        owned
            .running
            .handle
            .floor_recovery_receipt(reader, Hex32::new([81; 32])),
        Err(crate::ports::FloorRecoveryReadError::Forbidden)
    );
    assert_eq!(owned.fixture.entries(), before);
}

#[test]
fn subscription_advertises_recovery_only_with_running_control_and_recovery() {
    for (recovery, control) in [(false, false), (false, true), (true, false), (true, true)] {
        let owned = HttpFixture::new(recovery, control);
        let identity = owned
            .sessions
            .authorize_onboard(
                &owned.addr.to_string(),
                &format!("http://{}", owned.addr),
                &owned.exchange.cookie,
                &owned.exchange.csrf,
            )
            .unwrap();
        let opened = owned.running.handle.floor_subscribe(identity).unwrap();
        let snapshot: Value =
            serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
                .unwrap();
        assert_eq!(
            snapshot.get("recovery"),
            (recovery && control).then_some(&json!({}))
        );
    }
}

fn raw_reply(addr: SocketAddr, request: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    std::str::from_utf8(&reply)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn http_recovery_retains_exact_host_origin_csrf_and_framing_rejections() {
    let owned = HttpFixture::new(true, true);
    let before = owned.fixture.entries();
    for route in [
        "/floor/recovery/inspect",
        "/floor/recovery/submit",
        "/floor/recovery/receipt",
    ] {
        let valid = format!(
            "POST {route} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: dudes_floor_session={}\r\nX-Dudes-CSRF: {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}",
            owned.addr, owned.addr, owned.exchange.cookie, owned.exchange.csrf
        );
        for (request, expected) in [
            (
                valid.replace(&format!("Host: {}", owned.addr), "Host: localhost"),
                403,
            ),
            (
                valid.replace(
                    &format!("Origin: http://{}", owned.addr),
                    "Origin: http://localhost",
                ),
                403,
            ),
            (
                valid.replace(
                    "Content-Length: 2",
                    "Content-Length: 2\r\nContent-Length: 2",
                ),
                400,
            ),
            (
                valid.replace("Content-Type: application/json", "Content-Type: text/plain"),
                400,
            ),
            (
                valid.replace(
                    "Content-Length: 2",
                    "Transfer-Encoding: chunked\r\nContent-Length: 2",
                ),
                400,
            ),
            (
                valid.replace(
                    "Origin:",
                    &format!("Origin: http://{}\r\nOrigin:", owned.addr),
                ),
                400,
            ),
            (
                valid.replace(
                    "X-Dudes-CSRF:",
                    "X-Dudes-CSRF: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\nX-Dudes-CSRF:",
                ),
                400,
            ),
        ] {
            assert_eq!(raw_reply(owned.addr, request.as_bytes()), expected);
        }
    }
    assert_eq!(owned.fixture.entries(), before);
    assert!(owned.host.spawned().is_empty());
}

#[test]
fn actual_http_receipt_preserves_checked_store_unavailability() {
    let fixture = common::Fixture::new();
    let settings = floor_settings(&fixture, 0);
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let floor = daemon.floor.as_ref().unwrap();
    let addr = floor.addr();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let exchange = sessions
        .exchange(
            &addr.to_string(),
            &format!("http://{addr}"),
            sessions.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    let position = daemon.ctx.writer.next_lsn().unwrap();
    daemon
        .ctx
        .writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record: Record::Checkpoint {
                projection_version: 51,
                segment: position.segment(),
                offset: position.offset(),
            },
        })
        .unwrap();
    let before = fixture.entries();
    let mut running = common::start(daemon);
    let (code, view) = post(
        addr,
        &exchange.cookie,
        &exchange.csrf,
        "/floor/recovery/receipt",
        &serde_json::to_vec(&json!({"request_id": "82".repeat(32)})).unwrap(),
    );
    assert_eq!(code, 200);
    assert_eq!(view, json!({"state": "unavailable", "reason": "stale_cut"}));
    assert_eq!(fixture.entries(), before);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn actual_http_submission_reaches_one_result_through_the_existing_writer_continuation() {
    let fixture = common::Fixture::new();
    let settings = floor_settings(&fixture, 0);
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let agent = vanished_source(&mut daemon, &fixture);
    let floor = daemon.floor.as_ref().unwrap();
    let addr = floor.addr();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let exchange = sessions
        .exchange(
            &addr.to_string(),
            &format!("http://{addr}"),
            sessions.issue().unwrap().bootstrap.as_bytes(),
        )
        .unwrap();
    let host = daemon.ctx.host.clone();
    let mut running = common::start(daemon);
    let (code, inspection) = post(
        addr,
        &exchange.cookie,
        &exchange.csrf,
        "/floor/recovery/inspect",
        &serde_json::to_vec(&json!({"agent": agent})).unwrap(),
    );
    assert_eq!(code, 200);
    assert_eq!(inspection["state"], "available");
    let request = "83".repeat(32);
    let body = serde_json::to_vec(&json!({"request_id": request, "expected": inspection["expected"], "choice": "fresh_with_recap"})).unwrap();
    let (code, accepted) = post(
        addr,
        &exchange.cookie,
        &exchange.csrf,
        "/floor/recovery/submit",
        &body,
    );
    assert_eq!(code, 200);
    assert_eq!(accepted["state"], "accepted");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (code, view) = post(
            addr,
            &exchange.cookie,
            &exchange.csrf,
            "/floor/recovery/receipt",
            &serde_json::to_vec(&json!({"request_id": request})).unwrap(),
        );
        assert_eq!(code, 200);
        assert_eq!(view["state"], "found");
        if view["receipt"]["status"]["state"] == "consumed" {
            assert_eq!(view["receipt"]["request_id"], request);
            assert!(view["receipt"]["status"]["evidence_lsn"].is_string());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{view}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host.spawned().len(), 1);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::RecoveryAuthorized(_)))
            .count(),
        1
    );
    running.stop(crate::ExitCause::Ended);
}
