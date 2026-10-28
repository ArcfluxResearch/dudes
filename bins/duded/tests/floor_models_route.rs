//! `POST /floor/models/refresh`, driven through the real Floor gateway against a real daemon.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, SharedHost, TestClock};
use duded::floor_browser::Page;
use duded::settings::{FloorControl, FloorSettings};
use duded::startup::Witness;
use dudes_supervisor::Clock as _;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v10"
);
const ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

struct Floor {
    running: common::Running,
    settings: duded::Settings,
    addr: SocketAddr,
    clock: Arc<TestClock>,
}

fn floor(fixture: &Fixture, control: FloorControl) -> Floor {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_control = control;
    let clock = Arc::new(TestClock::new());
    let seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(common::ScriptedRunner::default()),
        clock.clone(),
        Witness::default(),
    );
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    Floor {
        running: common::start(daemon),
        settings,
        addr,
        clock,
    }
}

fn on() -> FloorControl {
    FloorControl::On {
        goal: false,
        retire: false,
        renew: false,
    }
}

fn session(floor: &Floor) -> Page {
    let clock = Arc::clone(&floor.clock);
    Page::enrol_new_on(
        floor.addr,
        &common::floor_link(&floor.settings),
        move || clock.now().unix_millis(),
    )
}

fn refresh(floor: &Floor, page: &Page, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let signed = page.signed("POST", "/floor/models/refresh", body.as_bytes());
    let host = floor.addr.to_string();
    let request = format!(
        "POST /floor/models/refresh HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCookie: dudes_floor_session={}\r\nX-Dudes-Origin: {}\r\nX-Dudes-Time: {}\r\nX-Dudes-Request: {}\r\nX-Dudes-Signature: {}\r\n\r\n{body}",
        body.len(),
        page.cookie,
        signed.origin,
        signed.time,
        signed.request,
        signed.signature
    );
    let mut stream = TcpStream::connect(floor.addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    stream.write_all(request.as_bytes()).expect("request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn a_signed_in_page_refreshes_a_list_and_nothing_is_journaled() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, on());
    let page = session(&floor);
    let before = fixture.records().len();

    let (status, body) = refresh(
        &floor,
        &page,
        &json!({"request_id": ID, "provider": "claude"}),
    );
    assert_eq!(
        (status, body),
        (
            200,
            json!({"request_id": ID, "state": "refreshed", "provider": "claude"})
        )
    );
    let (status, body) = refresh(&floor, &page, &json!({"request_id": ID, "provider": "pi"}));
    assert_eq!(
        (status, body),
        (
            409,
            json!({"request_id": ID, "state": "refused", "reason": "engine_absent"})
        ),
        "a program that is not installed has no list"
    );
    assert_eq!(
        refresh(
            &floor,
            &page,
            &json!({"request_id": ID, "provider": "gemini"})
        )
        .0,
        400
    );
    assert_eq!(
        fixture.records().len(),
        before,
        "a refresh appends no record"
    );
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn the_route_is_not_served_while_control_is_off() {
    let fixture = Fixture::new();
    let mut floor = floor(&fixture, FloorControl::Off);
    let page = session(&floor);
    let (status, _) = refresh(
        &floor,
        &page,
        &json!({"request_id": ID, "provider": "claude"}),
    );
    assert_ne!(status, 200);
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}
