//! `POST /floor/chat/file`, driven through the real Floor gateway with signed requests.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};
use duded::floor_browser::Page;
use duded::ports::Ask;
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_proto::Record;
use dudes_supervisor::Clock as _;
use dudes_types::AgentId;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v8"
);
const TEN_MIB: usize = 10 * 1024 * 1024;

struct Floor {
    running: common::Running,
    page: Page,
    addr: SocketAddr,
}

fn floor(fixture: &Fixture) -> Floor {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    let clock = Arc::new(TestClock::new());
    let seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::scripted([])),
        clock.clone(),
        Witness::default(),
    );
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    let running = common::start(daemon);
    let link = common::floor_link(&settings);
    let page = Page::enrol_new_on(addr, &link, move || clock.now().unix_millis());
    Floor {
        running,
        page,
        addr,
    }
}

fn finish(mut floor: Floor) {
    floor.running.stop(duded::ExitCause::ShutdownRpc);
}

fn encode(name: &str) -> String {
    name.bytes().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "%{byte:02X}");
        out
    })
}

fn target(agent: AgentId, name: &str) -> String {
    format!("/floor/chat/file?agent={agent}&name={}", encode(name))
}

fn head(page: &Page, target: &str, length: usize, signature: &str, content_type: &str) -> Vec<u8> {
    let host = page.addr.to_string();
    format!(
        "POST {target} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={}\r\n{signature}Content-Type: {content_type}\r\nContent-Length: {length}\r\n\r\n",
        page.cookie
    )
    .into_bytes()
}

fn upload(page: &Page, target: &str, body: &[u8]) -> common::Reply {
    let signed = page.signed("POST", target, body);
    let mut request = head(
        page,
        target,
        body.len(),
        &signed.header_lines(),
        "application/octet-stream",
    );
    request.extend_from_slice(body);
    common::send(page.addr, &request)
}

fn files_dir(fixture: &Fixture, agent: AgentId) -> PathBuf {
    fixture
        .state
        .path()
        .join("agents")
        .join(agent.to_string())
        .join("files")
}

fn is_copy_name(path: &Path, clean: &str) -> bool {
    let file = path.file_name().unwrap().to_str().unwrap();
    file.len() == 17 + clean.len()
        && file[..16]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && file[16..] == format!("-{clean}")
}

#[test]
fn a_signed_upload_keeps_the_exact_bytes_and_adds_no_journal_record() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let before = fixture.records().len();
    let bytes: Vec<u8> = (0..=255).cycle().take(70_000).collect();

    let reply = upload(&floor.page, &target(agent, "shot.png"), &bytes);

    assert_eq!(reply.status(), 200, "{}", reply.head);
    let answer = reply.json();
    assert_eq!(answer["name"], "shot.png");
    assert_eq!(answer["bytes"], 70_000);
    let path = PathBuf::from(answer["path"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), files_dir(&fixture, agent));
    assert!(is_copy_name(&path, "shot.png"), "{}", path.display());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(files_dir(&fixture, agent))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::read_dir(files_dir(&fixture, agent))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(fixture.records().len(), before);
    finish(floor);
}

#[test]
fn names_are_cleaned_to_ascii_with_no_path_in_them() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    for (given, clean) in [
        ("../../etc/passwd", "etcpasswd"),
        ("my résumé (final).pdf", "myrsumfinal.pdf"),
        (".bashrc", "bashrc"),
        ("notes.txt.", "notes.txt"),
        ("...", "file"),
        ("", "file"),
        (&"a".repeat(200), &"a".repeat(80)),
    ] {
        let reply = upload(&floor.page, &target(agent, given), b"x");
        assert_eq!(reply.status(), 200, "{given}: {}", reply.head);
        let answer = reply.json();
        assert_eq!(answer["name"], clean);
        let path = PathBuf::from(answer["path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), files_dir(&fixture, agent));
        assert!(is_copy_name(&path, clean), "{}", path.display());
    }
    finish(floor);
}

#[test]
fn a_length_over_ten_mib_is_refused_before_the_body_is_read() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let target = target(agent, "big.bin");
    let signed = floor.page.signed("POST", &target, b"");
    let request = head(
        &floor.page,
        &target,
        TEN_MIB + 1,
        &signed.header_lines(),
        "application/octet-stream",
    );

    let reply = common::send(floor.addr, &request);

    assert_eq!(reply.status(), 413, "{}", reply.head);
    assert!(!files_dir(&fixture, agent).exists());
    finish(floor);
}

#[test]
fn a_folder_at_its_limit_refuses_and_names_the_folder() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let folder = files_dir(&fixture, agent);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::File::create(folder.join("old.bin"))
        .unwrap()
        .set_len(200 * 1024 * 1024 - 4)
        .unwrap();

    let fits = upload(&floor.page, &target(agent, "a.txt"), b"1234");
    assert_eq!(fits.status(), 200, "{}", fits.head);
    let over = upload(&floor.page, &target(agent, "b.txt"), b"1");

    assert_eq!(over.status(), 409);
    assert_eq!(
        over.json(),
        json!({"state": "refused", "reason": "folder_full", "folder": folder.to_str().unwrap()})
    );
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 2);
    finish(floor);
}

#[test]
fn unknown_retired_and_attached_agents_and_bad_requests_are_refused() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let refused = |agent: AgentId| -> Value {
        let reply = upload(&floor.page, &target(agent, "a.txt"), b"x");
        assert_eq!(reply.status(), 409, "{}", reply.head);
        reply.json()
    };
    assert_eq!(
        refused(AgentId::NIL),
        json!({"state": "refused", "reason": "agent_absent"})
    );
    let retired = common::hire(&floor.running.handle, fixture.state.path());
    floor
        .running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent: retired,
                reason: "retire fixture".into(),
            },
        )))
        .unwrap();
    assert_eq!(
        refused(retired),
        json!({"state": "refused", "reason": "agent_retired"})
    );

    let live = common::hire(&floor.running.handle, fixture.state.path());
    let unsigned = {
        let target = target(live, "a.txt");
        let mut request = head(&floor.page, &target, 1, "", "application/octet-stream");
        request.push(b'x');
        common::send(floor.addr, &request)
    };
    assert_eq!(unsigned.status(), 401, "{}", unsigned.head);
    let text = {
        let target = target(live, "a.txt");
        let signed = floor.page.signed("POST", &target, b"x");
        let mut request = head(
            &floor.page,
            &target,
            1,
            &signed.header_lines(),
            "application/json",
        );
        request.push(b'x');
        common::send(floor.addr, &request)
    };
    assert_eq!(text.status(), 400, "{}", text.head);
    let no_agent = upload(&floor.page, "/floor/chat/file?name=a.txt", b"x");
    assert_eq!(no_agent.status(), 400);
    assert!(!files_dir(&fixture, live).exists());

    finish(floor);
}

#[test]
fn an_attached_agent_is_refused() {
    let fixture = Fixture::new();
    let first = floor(&fixture);
    let agent = common::hire(&first.running.handle, fixture.state.path());
    finish(first);
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions::default(),
    )
    .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_800_000),
            record: Record::Attached {
                agent,
                by: dudes_proto::Actor::System,
                hook_port: None,
                provenance: None,
            },
        })
        .unwrap();
    drop(writer);

    let second = floor(&fixture);
    let reply = upload(&second.page, &target(agent, "a.txt"), b"x");

    assert_eq!(reply.status(), 409, "{}", reply.head);
    assert_eq!(
        reply.json(),
        json!({"state": "refused", "reason": "attached"})
    );
    assert!(!files_dir(&fixture, agent).exists());
    finish(second);
}

#[test]
fn a_files_link_to_another_folder_is_refused_and_nothing_is_written_there() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let link = files_dir(&fixture, agent);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), link).unwrap();

    let reply = upload(&floor.page, &target(agent, "a.txt"), b"x");

    assert_eq!(reply.status(), 409);
    assert_eq!(
        reply.json(),
        json!({"state": "refused", "reason": "not_a_folder"})
    );
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    finish(floor);
}

#[test]
fn retiring_deletes_the_folder_and_a_restart_finishes_a_missed_delete() {
    let fixture = Fixture::new();
    let first = floor(&fixture);
    let agent = common::hire(&first.running.handle, fixture.state.path());
    let reply = upload(&first.page, &target(agent, "a.txt"), b"x");
    assert_eq!(reply.status(), 200);
    let folder = files_dir(&fixture, agent);
    assert!(folder.is_dir());

    first
        .running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "retire fixture".into(),
            },
        )))
        .unwrap();

    assert!(!folder.exists());
    assert!(folder.parent().unwrap().is_dir());
    finish(first);
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("left-behind"), b"x").unwrap();

    let second = floor(&fixture);

    assert!(!folder.exists());
    finish(second);
}

#[test]
fn a_second_upload_while_one_is_being_read_is_told_it_is_busy() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let slow = target(agent, "slow.bin");
    let signed = floor.page.signed("POST", &slow, b"");
    // The holder sends a head that promises 100 bytes and then 10 of them. If it hears an answer
    // the slot was not its to take, so it tries again; silence means the daemon is waiting for it.
    let holder = (0..10)
        .find_map(|_| {
            let mut holder = TcpStream::connect(floor.addr).unwrap();
            holder
                .write_all(&head(
                    &floor.page,
                    &slow,
                    100,
                    &signed.header_lines(),
                    "application/octet-stream",
                ))
                .unwrap();
            holder.write_all(b"0123456789").unwrap();
            holder
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let mut heard = [0_u8; 1];
            holder.read(&mut heard).is_err().then_some(holder)
        })
        .expect("the daemon waits for the holder's body");

    let busy = upload(&floor.page, &target(agent, "b.bin"), b"x");
    assert_eq!(busy.status(), 200, "{}", busy.head);
    assert_eq!(busy.json(), json!({"busy": true}));
    drop(holder);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let reply = upload(&floor.page, &target(agent, "c.bin"), b"x");
        if reply.status() == 200 && reply.json().get("path").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "never free: {}", reply.head);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_dir(files_dir(&fixture, agent))
            .unwrap()
            .count(),
        1
    );
    finish(floor);
}

fn send_signed_for(page: &Page, signed_for: &str, sent_to: &str, body: &[u8]) -> common::Reply {
    let signed = page.signed("POST", signed_for, body);
    let mut request = head(
        page,
        sent_to,
        body.len(),
        &signed.header_lines(),
        "application/octet-stream",
    );
    request.extend_from_slice(body);
    common::send(page.addr, &request)
}

#[test]
fn a_request_signed_for_another_target_or_body_is_refused_and_writes_nothing() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let other = common::hire(&floor.running.handle, fixture.state.path());
    let before = fixture.records().len();
    let signed_for = target(agent, "a.png");

    let wrong_name = send_signed_for(&floor.page, &signed_for, &target(agent, "b.png"), b"same");
    let wrong_agent = send_signed_for(&floor.page, &signed_for, &target(other, "a.png"), b"same");
    // The signature covers the body, so sign one and send another of the same length.
    let signed = floor.page.signed("POST", &signed_for, b"same");
    let mut request = head(
        &floor.page,
        &signed_for,
        4,
        &signed.header_lines(),
        "application/octet-stream",
    );
    request.extend_from_slice(b"samf");
    let altered = common::send(floor.addr, &request);

    for (label, reply) in [
        ("name", &wrong_name),
        ("agent", &wrong_agent),
        ("body", &altered),
    ] {
        assert_eq!(reply.status(), 401, "{label}: {}", reply.head);
    }
    assert!(!files_dir(&fixture, agent).exists());
    assert!(!files_dir(&fixture, other).exists());
    assert_eq!(fixture.records().len(), before);
    finish(floor);
}

#[test]
fn a_refused_upload_frees_the_slot_for_the_next_one() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let big = target(agent, "big.bin");
    let signed = floor.page.signed("POST", &big, b"");
    let too_big = common::send(
        floor.addr,
        &head(
            &floor.page,
            &big,
            TEN_MIB + 1,
            &signed.header_lines(),
            "application/octet-stream",
        ),
    );
    assert_eq!(too_big.status(), 413, "{}", too_big.head);
    let missing = upload(&floor.page, &target(AgentId::NIL, "a.txt"), b"x");
    assert_eq!(missing.status(), 409, "{}", missing.head);

    let next = upload(&floor.page, &target(agent, "a.txt"), b"x");

    assert_eq!(next.status(), 200, "{}", next.head);
    assert!(next.json().get("path").is_some());
    finish(floor);
}

#[test]
fn an_agent_retired_after_the_write_loses_the_copy_and_the_upload_is_refused() {
    let fixture = Fixture::new();
    let floor = floor(&fixture);
    let agent = common::hire(&floor.running.handle, fixture.state.path());
    let folder = files_dir(&fixture, agent);
    let (reached, release) = floor.running.handle.hold_after_chat_file_write();

    let reply = std::thread::scope(|scope| {
        let sending = scope.spawn(|| upload(&floor.page, &target(agent, "late.txt"), b"x"));
        reached
            .recv_timeout(Duration::from_secs(5))
            .expect("the write finished");
        floor
            .running
            .handle
            .ask(Ask::Retire(Box::new(
                dudes_proto::rpc::agent::RetireParams {
                    agent,
                    reason: "retire fixture".into(),
                },
            )))
            .unwrap();
        // The retirement's delete ran before the upload's write landed in the real order, so put
        // the copy back the way that write would have left it.
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("0123456789abcdef-late.txt"), b"x").unwrap();
        release.send(()).unwrap();
        sending.join().unwrap()
    });

    assert_eq!(reply.status(), 409, "{}", reply.head);
    assert_eq!(
        reply.json(),
        json!({"state": "refused", "reason": "agent_retired"})
    );
    assert!(!folder.exists());
    finish(floor);
}
