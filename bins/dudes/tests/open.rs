//! `dudes open` mints a one-time Floor link and hands it to the system's opener as one argument.
//! The opener here is a script that records its argv, the daemon is the real one on a scratch
//! home, and the test spends the recorded link the way a browser does.
#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duded::settings::{FloorSettings, Settings};
use dudes::exit;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../duded/tests/fixtures/floor-release-v3"
);

fn with_floor(fixture: &Fixture) -> Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings
}

/// An opener that writes each argument it was given on a line of `record`, then exits with `status`.
fn opener(dir: &Path, status: u8) -> (PathBuf, PathBuf) {
    let record = dir.join("argv");
    let script = dir.join("opener.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor argument in \"$@\"; do printf '%s\\n' \"$argument\" >> '{}'; done\nexit {status}\n",
            record.display()
        ),
    )
    .expect("an opener script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("executable");
    (script, record)
}

fn open(settings: &Settings, opener: &Path) -> common::Run {
    Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("DUDES_STATE_DIR", &settings.state_dir)
        .env("DUDES_OPENER", opener)
        .args(["open"])
        .run()
}

/// One POST as a browser sends it, answered with the status line, head and body.
fn post(authority: &str, path: &str, content_type: &str, headers: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(authority).expect("the Floor listens");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

#[test]
fn dudes_open_hands_one_link_to_the_opener_and_the_link_signs_a_session_in() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let _daemon = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (script, record) = opener(scratch.path(), 0);

    let run = open(&settings, &script);

    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    let recorded = std::fs::read_to_string(&record).expect("the opener ran");
    let arguments: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        arguments.len(),
        1,
        "one argument and nothing else: {recorded:?}"
    );
    let link = arguments[0];
    let (page, bootstrap) = link.split_once("#bootstrap=").expect("a sign-in link");
    let authority = page
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/floor/"))
        .expect("the loopback Floor page");
    assert!(
        !run.output().contains(bootstrap),
        "the link reaches only the opener: {}",
        run.output()
    );
    assert!(
        run.stdout.contains("Opened Dudes in your browser."),
        "{}",
        run.stdout
    );

    let signed_in = post(
        authority,
        "/floor/session",
        "text/plain;charset=UTF-8",
        "",
        bootstrap,
    );
    assert!(signed_in.starts_with("HTTP/1.1 200 OK\r\n"), "{signed_in}");
    let cookie = signed_in
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|rest| rest.split(';').next())
        .expect("a session cookie");
    let secret = signed_in
        .split("\"csrf\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the page secret");
    let acts = post(
        authority,
        "/floor/session/check",
        "application/json",
        &format!("Cookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {secret}\r\n"),
        "{}",
    );
    assert!(
        acts.starts_with("HTTP/1.1 200 OK\r\n"),
        "the session acts: {acts}"
    );
    assert!(acts.ends_with(r#"{"session":"live"}"#), "{acts}");

    let again = post(
        authority,
        "/floor/session",
        "text/plain;charset=UTF-8",
        "",
        bootstrap,
    );
    assert!(
        again.starts_with("HTTP/1.1 403"),
        "a link works once: {again}"
    );
}

#[test]
fn an_opener_that_fails_leaves_the_link_printed_once_for_pasting() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let _daemon = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (script, _) = opener(scratch.path(), 1);

    let run = open(&settings, &script);

    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    assert!(run.stderr.contains("not opened"), "{}", run.stderr);
    assert_eq!(
        run.stdout.matches("#bootstrap=").count(),
        1,
        "{}",
        run.stdout
    );
    assert!(!run.stderr.contains("#bootstrap="), "{}", run.stderr);
}
