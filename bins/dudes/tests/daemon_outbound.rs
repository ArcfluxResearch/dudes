//! A running daemon holds no socket that leaves the machine.
//!
//! The release gate scans the shipped binaries' dependencies for network clients. This checks the
//! process itself: a real `duded` is started in scratch directories, driven through boot, the
//! Floor sign-in link and page, a hire and one stand-in engine turn, and after each step the
//! daemon's internet sockets are listed with `lsof`. Each must be a listener on a loopback
//! address or a connection with loopback at both ends.
//!
//! It proves the daemon process held no non-loopback socket at the sampled moments. It does not
//! see a connection opened and closed between two samples, or one made by an engine child.
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod common;

use common::{Cli, Lifeline, run_program};

const SPAWN_MARKER: &str = "quickstart started duded pid ";
const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../duded/tests/fixtures/floor-release-v10"
);

fn sibling_daemon() -> PathBuf {
    let daemon = PathBuf::from(env!("CARGO_BIN_EXE_dudes")).with_file_name("duded");
    assert!(
        daemon.is_file(),
        "{} is absent; run `cargo build -p duded --locked` first",
        daemon.display()
    );
    daemon
}

fn lsof() -> PathBuf {
    ["/usr/sbin/lsof", "/usr/bin/lsof", "/sbin/lsof", "/bin/lsof"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("lsof is not installed; this check cannot run without it")
}

/// Every internet socket `pid` holds, as `lsof` names it (`127.0.0.1:80->127.0.0.1:9`, `*:80`).
fn sockets(pid: u32) -> Vec<String> {
    let args: Vec<OsString> = ["-nP", "-a", "-p", &pid.to_string(), "-i", "-Fn"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let run = run_program(&lsof(), &args);
    // lsof exits 1 with no output when the process holds no internet socket.
    assert!(
        run.code == Some(0) || (run.code == Some(1) && run.stderr.is_empty()),
        "lsof failed: {run:?}"
    );
    run.stdout
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(str::to_owned)
        .collect()
}

fn is_loopback(end: &str) -> bool {
    let host = end.rsplit_once(':').map_or(end, |(host, _)| host);
    host.starts_with("127.") || host == "[::1]"
}

/// Assert every socket of `pid` stays on the loopback, naming the offending line.
fn assert_loopback_only(pid: u32, moment: &str) -> usize {
    let held = sockets(pid);
    for line in &held {
        assert!(
            line.split("->").all(is_loopback),
            "duded {pid} held a non-loopback socket {moment}: {line}\nall sockets: {held:?}"
        );
    }
    held.len()
}

fn daemon_pid(root: &Path) -> u32 {
    std::fs::read_to_string(root.join("run/duded.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix(SPAWN_MARKER)?.parse().ok())
        .next_back()
        .expect("the daemon log names the pid quickstart started")
}

#[test]
fn the_daemon_holds_only_loopback_sockets_while_it_works() {
    let root = tempfile::tempdir().expect("a root");
    let _lifeline = Lifeline::start(root.path());
    let bin = root.path().join("bin");
    let home = root.path().join("home");
    let work = root.path().join("work");
    for dir in [bin.join("floor/assets"), home.clone(), work.clone()] {
        std::fs::create_dir_all(dir).expect("a scratch directory");
    }
    let daemon = bin.join("duded");
    std::fs::copy(sibling_daemon(), &daemon).expect("install duded");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        std::fs::copy(
            Path::new(FLOOR_FIXTURE).join(path),
            bin.join("floor").join(path),
        )
        .expect("copy the Floor bundle");
    }
    let claude = bin.join("claude");
    let ran = root.path().join("engine-ran");
    std::fs::write(&claude, format!("#!/bin/sh\n: > '{}'\n", ran.display()))
        .expect("write the stand-in engine");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700))
        .expect("make the stand-in executable");
    let path = std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
        .expect("a search path");
    let cli = || {
        Cli::bare()
            .env("HOME", &home)
            .env("PATH", &path)
            .env("DUDES_DAEMON", &daemon)
            .env("DUDES_STATE_DIR", root.path().join("state"))
            .env("DUDES_RUNTIME_DIR", root.path().join("run"))
    };

    let started = cli().arg("quickstart").run();
    assert_eq!(started.code(), 0, "{started:?}");
    let pid = daemon_pid(root.path());
    let listening = assert_loopback_only(pid, "after boot");
    assert!(
        listening > 0,
        "lsof saw no Floor listener, so it is not looking at the daemon"
    );

    let opened = cli().args(["floor", "open"]).run();
    assert_eq!(opened.code(), 0, "{opened:?}");
    let link = opened.stdout.lines().next().unwrap_or_default();
    let origin = link
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .expect("the sign-in link's host and port");
    let mut page = std::net::TcpStream::connect(origin).expect("the Floor listener");
    std::io::Write::write_all(
        &mut page,
        format!("GET /floor/ HTTP/1.1\r\nHost: {origin}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .expect("ask for the page");
    let mut served = String::new();
    std::io::Read::read_to_string(&mut page, &mut served).expect("read the page");
    assert!(served.starts_with("HTTP/1.1 200 OK"), "{served}");
    assert_loopback_only(pid, "after the Floor page was served");

    let hired = cli().arg("--json").args(common::hire_args(&work)).run();
    assert_eq!(hired.code(), 0, "{hired:?}");
    let agent = hired
        .json::<dudes_proto::rpc::agent::HireResult>()
        .agent
        .to_string();
    assert_loopback_only(pid, "after a hire");

    let goal = cli()
        .args(common::goal_args(&agent, "reply with the single word ok"))
        .run();
    assert_eq!(goal.code(), 0, "{goal:?}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ran.exists() {
        assert_loopback_only(pid, "during a turn");
        assert!(Instant::now() < deadline, "the stand-in engine did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_loopback_only(pid, "after a turn");

    let stopped = cli().args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), 0, "{stopped:?}");
}

#[test]
fn only_loopback_ends_pass_the_check() {
    for ok in [
        "127.0.0.1:8080",
        "[::1]:8080",
        "127.0.0.1:50000->127.0.0.1:8080",
    ] {
        assert!(ok.split("->").all(is_loopback), "{ok}");
    }
    for bad in [
        "*:8080",
        "10.0.0.5:8080",
        "[::]:8080",
        "127.0.0.1:50000->93.184.216.34:443",
    ] {
        assert!(!bad.split("->").all(is_loopback), "{bad}");
    }
}
