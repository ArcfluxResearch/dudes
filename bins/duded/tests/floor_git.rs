//! `POST /floor/git`, driven through the real Floor gateway against a real daemon,
//! the real one-shot runner and real git.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, SharedHost, TestClock};
use duded::floor_session::CSRF_HEADER_NAME;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::settings::FloorSettings;
use duded::startup::Witness;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::Record;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::agent::{Checkout, HireResult};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_rpc::Client;
use dudes_types::AgentId;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
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
}

fn floor(fixture: &Fixture, enabled: bool, git_path: Option<OsString>) -> Floor {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_git_enabled = enabled;
    settings.predicate_path = git_path.or_else(|| std::env::var_os("PATH"));
    let seams = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(Real(OneShot::new())),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    Floor {
        running: common::start(daemon),
        settings,
        addr,
    }
}

fn credential(settings: &duded::Settings) -> String {
    std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential")
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

/// The cookie and the CSRF nonce of a signed-in page.
fn session(floor: &Floor) -> (String, String) {
    let mut client = Client::connect(floor.settings.socket()).expect("daemon socket");
    let reply = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(credential(&floor.settings)).expect("credential"),
        )
        .expect("issue response");
    let issued: SessionIssueResult =
        serde_json::from_value(reply.result.expect("issue succeeded")).expect("typed issue");
    let host = floor.addr.to_string();
    let mut stream = TcpStream::connect(floor.addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{}",
        issued.bootstrap.len(),
        issued.bootstrap
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

fn post(floor: &Floor, cookie: &str, csrf: &str, agent: AgentId) -> Reply {
    post_body(
        floor,
        cookie,
        csrf,
        &json!({ "agent_id": agent.to_string() }).to_string(),
    )
}

fn post_body(floor: &Floor, cookie: &str, csrf: &str, body: &str) -> Reply {
    post_raw(
        floor,
        &[
            ("Cookie", format!("dudes_floor_session={cookie}")),
            (CSRF_HEADER_NAME, csrf.to_owned()),
        ],
        body,
    )
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
    let (cookie, csrf) = session(&floor);
    let before = records(&fixture);

    let reply = post(&floor, &cookie, &csrf, agent);
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
    let (cookie, csrf) = session(&floor);
    let reply = post(&floor, &cookie, &csrf, agent);
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
    let (cookie, csrf) = session(&floor);
    let body = json!({ "agent_id": agent.to_string() }).to_string();
    let cookie_header = ("Cookie", format!("dudes_floor_session={cookie}"));

    assert_eq!(post_raw(&floor, &[], &body).status, 403, "no cookie");
    assert_eq!(
        post_raw(&floor, std::slice::from_ref(&cookie_header), &body).status,
        403,
        "no CSRF"
    );
    let wrong = [cookie_header.clone(), (CSRF_HEADER_NAME, "A".repeat(43))];
    assert_eq!(
        post_raw(&floor, &wrong, &body).status,
        403,
        "the wrong CSRF"
    );
    let foreign = [
        ("Cookie", "dudes_floor_session=forged".to_owned()),
        (CSRF_HEADER_NAME, csrf.clone()),
    ];
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
        " ".repeat(129),
    ] {
        let reply = post_body(&floor, &cookie, &csrf, &bad);
        assert_eq!(
            reply.status,
            if bad.len() > 128 { 413 } else { 400 },
            "{bad:?}: {}",
            reply.head
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let ok = post(&floor, &cookie, &csrf, agent);
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
fn unknown_agents_and_unreadable_directories_are_finite_states() {
    let fixture = Fixture::new();
    let root = std::fs::canonicalize(fixture.runtime.path()).expect("canonical");
    let plain = root.join("plain");
    std::fs::create_dir(&plain).expect("plain directory");
    let linked = root.join("linked");
    std::fs::create_dir(&linked).expect("linked directory");
    std::fs::write(linked.join(".git"), "gitdir: /elsewhere/.git\n").expect("gitfile");
    let mut floor = floor(&fixture, true, None);
    let (cookie, csrf) = session(&floor);
    let state = |agent: AgentId| {
        let reply = post(&floor, &cookie, &csrf, agent);
        assert_eq!(reply.status, 200, "{}", reply.head);
        std::thread::sleep(Duration::from_millis(2100));
        reply.json()["state"].as_str().expect("a state").to_owned()
    };
    assert_eq!(
        state(common::hire(&floor.running.handle, &plain)),
        "not_a_checkout_root"
    );
    assert_eq!(
        state(common::hire(&floor.running.handle, &linked)),
        "gitfile_unsupported"
    );
    let stranger = AgentId::parse("01J00000000000000000000000").expect("an id");
    assert_eq!(state(stranger), "agent_unknown");
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
    let (cookie, csrf) = session(&floor);
    assert_eq!(
        post(&floor, &cookie, &csrf, agent).json()["state"],
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
    assert_eq!(
        post(&floor, &cookie, &csrf, agent).json()["state"],
        "agent_unknown"
    );
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// A worktree agent's tab reads its tree through the lease while the agent works, measured from
/// the commit the tree started at. Once the agent retires, the tab answers the tree's state and
/// reads nothing, and no reply names the repository, the tree or the lease id.
#[test]
fn a_worktree_agent_reads_through_its_lease_and_answers_its_state_once_retired() {
    let fixture = Fixture::new();
    let origin = repo(fixture.runtime.path());
    git(&origin, &["checkout", "--quiet", "--", "."]);
    let mut floor = floor(&fixture, true, None);
    let mut params = common::hire_params(&origin);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    let mut client = Client::connect(floor.settings.socket()).expect("daemon socket");
    let hired = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(&params).expect("params")),
            OperatorCredential::new(credential(&floor.settings)).expect("credential"),
        )
        .expect("agent.hire is answered");
    let agent = serde_json::from_value::<HireResult>(hired.result.expect("a worktree hire"))
        .expect("a hire result")
        .agent;
    let (worktree, tree) = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired {
                id,
                cwd,
                worktree: Some(worktree),
                ..
            } if id == agent => Some((worktree, cwd)),
            _ => None,
        })
        .expect("the hire recorded its worktree");
    std::fs::write(tree.join("agent.txt"), "work\n").expect("the agent's file");
    git(&tree, &["add", "-A"]);
    git(&tree, &["commit", "--quiet", "-m", "agent work"]);
    let (cookie, csrf) = session(&floor);
    let hidden = [
        origin.display().to_string(),
        tree.display().to_string(),
        worktree.to_string(),
    ];
    let read = |expected: &str| {
        let reply = post(&floor, &cookie, &csrf, agent);
        assert_eq!(reply.status, 200, "{}", reply.head);
        let wire = String::from_utf8_lossy(&reply.body).into_owned();
        for name in &hidden {
            assert!(!wire.contains(name.as_str()), "{name} in {wire}");
        }
        let view = reply.json();
        assert_eq!(view["state"], expected, "{view}");
        std::thread::sleep(Duration::from_millis(2100));
        view
    };

    let view = read("ok");
    assert_eq!(view["head"]["against"], "worktree_start");
    assert_eq!(
        view["head"]["branch"],
        format!("dudes/{}", worktree.short())
    );
    assert_eq!(
        (
            view["head"]["ahead"].as_u64(),
            view["head"]["behind"].as_u64()
        ),
        (Some(1), Some(0))
    );
    assert_eq!(view["commits"]["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["commits"]["items"][0]["subject"], "agent work");
    assert_eq!(view["committed"]["items"][0]["path"], "agent.txt");

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
    read("worktree_released");

    git(
        &origin,
        &["worktree", "remove", "--", tree.to_str().expect("utf-8")],
    );
    floor
        .running
        .handle
        .ask(duded::ports::Ask::WorktreeRemoved(worktree))
        .expect("the removal is recorded");
    read("worktree_removed");
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
    let (cookie, csrf) = session(&floor);
    let first = post(&floor, &cookie, &csrf, agent).json();
    let second = post(&floor, &cookie, &csrf, agent).json();
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
    assert_eq!(post(&floor, &cookie, &csrf, agent).json()["state"], "ok");
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// A `git` that answers its version and then never answers again: the read ends at the command
/// deadline with `timed_out` and no child is left, and a daemon asked to stop mid-read stops.
#[test]
fn a_git_that_never_answers_ends_in_a_deadline_and_does_not_hold_the_daemon() {
    let fixture = Fixture::new();
    let checkout = repo(fixture.runtime.path());
    let bin = std::fs::canonicalize(fixture.runtime.path())
        .expect("canonical")
        .join("slow-bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let script = bin.join("git");
    std::fs::write(
        &script,
        "#!/bin/sh\ncase \"$1\" in --version) echo git version 2.54.0;; *) /bin/sleep 60;; esac\n",
    )
    .expect("slow git");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // The first run of a new program can be slow to start on macOS, which would spend the version
    // check's deadline before the read began.
    let warm = LaunchSpec {
        argv: vec![script.clone().into_os_string(), OsString::from("--version")],
        env: Vec::new(),
        cwd: bin.clone(),
        stdin: LaunchInput::Null,
    };
    OneShot::new()
        .run(&warm, WAIT)
        .expect("the slow git starts");
    let mut floor = floor(&fixture, true, Some(bin.into_os_string()));
    let agent = common::hire(&floor.running.handle, &checkout);
    let (cookie, csrf) = session(&floor);

    let started = Instant::now();
    let reply = post(&floor, &cookie, &csrf, agent);
    assert_eq!(reply.json()["state"], "timed_out");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );

    std::thread::sleep(Duration::from_millis(2100));
    let host = floor.addr.to_string();
    let body = json!({ "agent_id": agent.to_string() }).to_string();
    let mut stream = TcpStream::connect(floor.addr).expect("Floor listener");
    stream.set_read_timeout(Some(WAIT)).expect("timeout");
    stream
        .write_all(
            format!(
                "POST /floor/git HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\nCookie: dudes_floor_session={cookie}\r\n{CSRF_HEADER_NAME}: {csrf}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("request");
    std::thread::sleep(Duration::from_millis(500));
    let stopping = Instant::now();
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    assert!(
        stopping.elapsed() < Duration::from_secs(8),
        "stop took {:?}",
        stopping.elapsed()
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
