#![cfg(unix)]

use std::time::{Duration, Instant};

use super::*;

fn arguments(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

#[test]
fn a_program_that_finishes_returns_its_output() {
    let output = run(
        Path::new("/bin/echo"),
        &arguments(&["hello"]),
        &[],
        Duration::from_secs(30),
    )
    .expect("echo runs");
    assert!(output.success && !output.timed_out);
    assert_eq!(output.stdout, b"hello\n");
}

#[test]
fn a_program_that_outlives_its_limit_is_stopped_and_reported() {
    let started = Instant::now();
    let output = run(
        Path::new("/bin/sleep"),
        &arguments(&["60"]),
        &[],
        Duration::from_millis(300),
    )
    .expect("sleep runs");
    assert!(output.timed_out && !output.success);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_missing_program_is_not_found() {
    let error = run(
        Path::new("/nonexistent/program"),
        &[],
        &[],
        Duration::from_secs(1),
    )
    .expect_err("nothing to run");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn only_the_given_environment_reaches_the_child() {
    let env = [EnvVar::new("DUDES_PROOF", "kept")];
    let output = run(
        Path::new("/usr/bin/env"),
        &[],
        &env,
        Duration::from_secs(30),
    )
    .expect("env runs");
    assert_eq!(output.stdout, b"DUDES_PROOF=kept\n");
}
