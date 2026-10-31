//! `POST /floor/git`, driven through the real Floor gateway against a real daemon,
//! the real one-shot runner and real git.
#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Fixture, SharedHost, TestClock};
use duded::floor_browser::{Browser, Page};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_supervisor::Clock as _;
use dudes_types::AgentId;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v11"
);
const WAIT: Duration = Duration::from_secs(20);

struct Real(OneShot);

impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|outcome| match outcome {
            OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            OneShotOutcome::Swept => PredicateOutcome::Swept,
        })
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
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
        self.0.sweep();
    }
}

struct Floor {
    running: common::Running,
    settings: duded::Settings,
    addr: SocketAddr,
    clock: Arc<TestClock>,
}

fn floor(fixture: &Fixture, enabled: bool, git_path: Option<OsString>) -> Floor {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_git_enabled = enabled;
    settings.predicate_path = git_path.or_else(|| std::env::var_os("PATH"));
    let clock = Arc::new(TestClock::new());
    let seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(Real(OneShot::new())),
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

/// A signed-in page, signing with the clock the daemon runs on.
fn session(floor: &Floor) -> Page {
    let clock = Arc::clone(&floor.clock);
    Page::enrol_new_on(
        floor.addr,
        &common::floor_link(&floor.settings),
        move || clock.now().unix_millis(),
    )
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("JSON body: {}", String::from_utf8_lossy(&self.body)))
    }
}

fn post_raw(floor: &Floor, headers: &[(&str, String)], body: &str) -> Reply {
    let host = floor.addr.to_string();
    let mut request = format!(
        "POST /floor/git HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&[name, ": ", value.as_str(), "\r\n"].concat());
    }
    request.push_str("\r\n");
    request.push_str(body);
    let mut stream = TcpStream::connect(floor.addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    stream.write_all(request.as_bytes()).expect("request");
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

fn post(floor: &Floor, page: &Page, agent: AgentId) -> Reply {
    post_body(
        floor,
        page,
        &json!({ "agent_id": agent.to_string() }).to_string(),
    )
}

fn post_body(floor: &Floor, page: &Page, body: &str) -> Reply {
    let signed = page.signed("POST", "/floor/git", body.as_bytes());
    post_raw(floor, &page_headers(page, Some(&signed)), body)
}

fn page_headers(
    page: &Page,
    signed: Option<&duded::floor_browser::Signed>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Cookie", format!("dudes_floor_session={}", page.cookie))];
    if let Some(signed) = signed {
        headers.extend([
            ("X-Dudes-Origin", signed.origin.clone()),
            ("X-Dudes-Time", signed.time.clone()),
            ("X-Dudes-Request", signed.request.clone()),
            ("X-Dudes-Signature", signed.signature.clone()),
        ]);
    }
    headers
}

/// Plain git for setup, through the same real runner.
fn git(dir: &Path, args: &[&str]) {
    let mut command = vec![OsString::from("git")];
    command.extend(args.iter().map(OsString::from));
    let program = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|bin| bin.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    command[0] = program.into_os_string();
    let spec = LaunchSpec {
        argv: command,
        env: vec![
            EnvVar::new("PATH", std::env::var_os("PATH").expect("PATH")),
            EnvVar::new("HOME", dir),
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            EnvVar::new("GIT_AUTHOR_NAME", "Lab"),
            EnvVar::new("GIT_AUTHOR_EMAIL", "lab@example.com"),
            EnvVar::new("GIT_COMMITTER_NAME", "Lab"),
            EnvVar::new("GIT_COMMITTER_EMAIL", "lab@example.com"),
        ],
        cwd: dir.to_path_buf(),
        stdin: LaunchInput::Null,
    };
    match OneShot::new()
        .run(&spec, Duration::from_secs(60))
        .expect("git runs")
    {
        OneShotOutcome::Finished { exit: Some(0), .. } => {}
        other => panic!("git {args:?}: {other:?}"),
    }
}

/// A repository with two commits and one uncommitted edit, outside the daemon's directories.
fn repo(root: &Path) -> PathBuf {
    let repo = std::fs::canonicalize(root)
        .expect("canonical")
        .join("checkout");
    std::fs::create_dir_all(&repo).expect("repo directory");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "first"]);
    std::fs::write(repo.join("tracked.txt"), "two\n").expect("edit");
    std::fs::write(repo.join("new.txt"), "x\n").expect("untracked");
    repo
}

fn records(fixture: &Fixture) -> usize {
    fixture.records().len()
}

#[test]
fn a_signed_in_page_reads_an_agents_checkout_and_nothing_is_journaled() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let mut floor = floor(&fixture, true, None);
    let agent = common::hire(&floor.running.handle, &checkout);
    let page = session(&floor);
    let before = records(&fixture);

    let reply = post(&floor, &page, agent);
    assert_eq!(reply.status, 200, "{}", reply.head);
    assert!(
        reply.head.contains("Content-Type: application/json"),
        "{}",
        reply.head
    );
    assert!(
        reply.head.contains("Cache-Control: no-store"),
        "{}",
        reply.head
    );
    let view = reply.json();
    assert_eq!(view["state"], "ok", "{view}");
    assert_eq!(view["head"]["branch"], "main");
    assert_eq!(view["head"]["untracked"], 1);
    assert_eq!(view["changes"]["items"][0]["path"], "tracked.txt");
    assert_eq!(view["commits"]["items"][0]["subject"], "first");
    let wire = String::from_utf8_lossy(&reply.body).into_owned();
    assert!(
        !wire.contains(&checkout.display().to_string()) && !wire.contains("new.txt"),
        "no working directory and no untracked name in {wire}"
    );
    assert_eq!(records(&fixture), before, "a Git read journals nothing");
    assert_eq!(
        std::fs::read_dir(floor.settings.runtime_dir.join("git-read")).map_or(0, Iterator::count),
        0,
        "the shadow is gone"
    );
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn the_route_is_absent_unless_the_operator_enabled_it() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let mut floor = floor(&fixture, false, None);
    let agent = common::hire(&floor.running.handle, &checkout);
    let page = session(&floor);
    let reply = post(&floor, &page, agent);
    assert_eq!(
        reply.status, 400,
        "an unrouted POST with a body, as any other: {}",
        reply.head
    );
    assert!(!reply.head.contains("application/json"), "{}", reply.head);
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn the_session_checks_come_before_any_read() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let mut floor = floor(&fixture, true, None);
    let agent = common::hire(&floor.running.handle, &checkout);
    let page = session(&floor);
    let body = json!({ "agent_id": agent.to_string() }).to_string();
    let signed = page.signed("POST", "/floor/git", body.as_bytes());

    assert_eq!(post_raw(&floor, &[], &body).status, 403, "no cookie");
    let unsigned = post_raw(&floor, &page_headers(&page, None), &body);
    assert_eq!(unsigned.status, 401, "no signature");
    assert_eq!(unsigned.json(), json!({ "signature": "refused" }));
    let other = Browser::new().sign(
        "POST",
        "/floor/git",
        &page.origin(),
        body.as_bytes(),
        page.now_ms(),
    );
    let wrong = post_raw(&floor, &page_headers(&page, Some(&other)), &body);
    assert_eq!(wrong.status, 401, "another browser's signature");
    assert_eq!(wrong.json(), json!({ "signature": "refused" }));
    let mut foreign = page_headers(&page, Some(&signed));
    foreign[0] = ("Cookie", "dudes_floor_session=forged".to_owned());
    assert_eq!(
        post_raw(&floor, &foreign, &body).status,
        403,
        "an unknown session"
    );

    for bad in [
        "{}".to_owned(),
        "[]".to_owned(),
        json!({ "agent_id": agent.to_string(), "path": "/etc" }).to_string(),
        json!({ "agent_id": "nope" }).to_string(),
        " ".repeat(duded::floor_git::MAX_GIT_BODY_BYTES + 1),
    ] {
        let reply = post_body(&floor, &page, &bad);
        assert_eq!(
            reply.status,
            if bad.len() > duded::floor_git::MAX_GIT_BODY_BYTES {
                413
            } else {
                400
            },
            "{bad:?}: {}",
            reply.head
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let ok = post(&floor, &page, agent);
    assert_eq!(
        ok.status, 200,
        "none of the refusals took the session's turn: {}",
        ok.head
    );
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn a_retired_agent_is_unknown_to_the_route() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let mut floor = floor(&fixture, true, None);
    let agent = common::hire(&floor.running.handle, &checkout);
    let page = session(&floor);
    assert_eq!(
        post(&floor, &page, agent).json()["state"],
        "ok",
        "control: a live agent reads"
    );
    floor
        .running
        .handle
        .ask(duded::ports::Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "the test retires it".into(),
            },
        )))
        .expect("retire");
    std::thread::sleep(Duration::from_millis(2100));
    assert_eq!(post(&floor, &page, agent).json()["state"], "agent_unknown");
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// One read at a time per session: a second request inside the interval is `busy` and leaves the
/// session's first answer untouched, and the next one after the interval reads again.
#[test]
fn a_session_may_read_once_per_interval() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let mut floor = floor(&fixture, true, None);
    let agent = common::hire(&floor.running.handle, &checkout);
    let page = session(&floor);
    let first = post(&floor, &page, agent).json();
    let second = post(&floor, &page, agent).json();
    assert_eq!(
        (first["state"].as_str(), second["state"].as_str()),
        (Some("ok"), Some("busy"))
    );
    assert!(
        second["head"].is_null()
            && second["changes"]["items"]
                .as_array()
                .is_some_and(Vec::is_empty)
    );
    std::thread::sleep(Duration::from_millis(2100));
    assert_eq!(post(&floor, &page, agent).json()["state"], "ok");
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn the_setting_is_off_unless_written_and_refuses_unknown_keys() {
    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let load = |text: &str| {
        std::fs::write(&config, text).expect("operator config");
        duded::config::load_operator(&config).map(|config| config.git_enabled)
    };
    assert!(!load("").expect("absent is off"));
    assert!(!load("[floor.git]\nenabled = false\n").expect("off"));
    assert!(load("[floor.git]\nenabled = true\n").expect("on"));
    assert!(
        load("[floor.git]\nenabled = true\nremote = true\n").is_err(),
        "an unknown key is refused"
    );
    assert!(!duded::Settings::clone(&common::Fixture::new().settings()).floor_git_enabled);
}
