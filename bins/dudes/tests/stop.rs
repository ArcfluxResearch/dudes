//! Hard-stop completion tests.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use dudes::exit;

mod common;

use common::{Cli, Fixture, OpenRunner, Run, SharedHost, ShutdownHold, start};

const CONTAINMENT_WARNING: &str = "dudes: warning: the daemon stopped and every agent process it \
    started has exited, but it cannot prove that nothing those processes started left their process \
    group or job\n";

/// The CLI's copy of the lock file's name is the daemon's.
#[test]
fn the_lock_name_matches_the_daemons() {
    assert_eq!(dudes::env::LOCK_NAME, duded::lock::LOCK_FILE_NAME);
}

/// The daemon answers `system.shutdown` before its exit sweeps the agents' processes. Holding
/// that sweep shows `stop` still waiting after the answer, and returning once the sweep ends.
#[test]
fn stop_returns_after_the_daemon_sweeps_its_agent_processes() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let hold = ShutdownHold::default();
    let _running = start(
        &settings,
        SharedHost::holding_shutdown(hold.clone()),
        Arc::new(OpenRunner::default()),
    );
    let stopped = stop_in_background(
        Cli::new(&settings.socket(), &settings.runtime_dir)
            .env("DUDES_STATE_DIR", &settings.state_dir),
    );
    assert!(
        hold.wait_entered(Duration::from_secs(10)),
        "the daemon never reached its host sweep"
    );
    if let Ok(early) = stopped.recv_timeout(Duration::from_millis(500)) {
        hold.release();
        panic!("stop returned while the daemon's exit was still running: {early:?}");
    }

    hold.release();
    let stopped = stopped
        .recv_timeout(Duration::from_secs(30))
        .expect("stop returned once the sweep ended");
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
    assert_eq!(stopped.stdout, "the daemon stopped\n");
    assert_eq!(
        stopped.stderr, "",
        "a daemon that started no agent process warns of nothing"
    );
}

#[test]
fn stop_observes_the_answering_daemon_despite_a_mismatched_state_directory() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let elsewhere = tempfile::tempdir().expect("another state directory");
    std::fs::File::create(elsewhere.path().join(dudes::env::LOCK_NAME))
        .expect("an unlocked lock file");
    let hold = ShutdownHold::default();
    let _running = start(
        &settings,
        SharedHost::holding_shutdown(hold.clone()),
        Arc::new(OpenRunner::default()),
    );
    let stopped = stop_in_background(
        Cli::new(&settings.socket(), &settings.runtime_dir)
            .env("DUDES_STATE_DIR", elsewhere.path()),
    );
    assert!(
        hold.wait_entered(Duration::from_secs(10)),
        "the host sweep starts"
    );
    if let Ok(early) = stopped.recv_timeout(Duration::from_millis(500)) {
        hold.release();
        panic!("the override let stop claim completion before cleanup: {early:?}");
    }
    hold.release();
    let stopped = stopped
        .recv_timeout(Duration::from_secs(10))
        .expect("completed stop");
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
    let _next = duded::lock::StateLock::take(&settings.state_dir)
        .expect("stop completion releases the state lock for the next start");
}

#[test]
fn stop_does_not_probe_or_wait_for_the_next_daemons_lock() {
    let dir = tempfile::tempdir().expect("a daemon's directories");
    let state = dir.path().join("state");
    std::fs::create_dir(&state).expect("a state directory");
    let lock = std::fs::File::create(state.join(dudes::env::LOCK_NAME)).expect("the lock file");
    lock.lock().expect("the stand-in daemon holds its lock");
    let receipt = dir.path().join("completion");
    std::fs::write(&receipt, "").expect("pending receipt");
    let socket = answer_one_shutdown(
        dir.path(),
        serde_json::json!({
            "completion": {"path": receipt, "run": "the-old-run"}
        }),
    );

    let stopped = stop_in_background(Cli::new(&socket, dir.path()).env("DUDES_STATE_DIR", &state));
    if let Ok(early) = stopped.recv_timeout(Duration::from_millis(800)) {
        panic!("stop returned while the daemon still held its lock: {early:?}");
    }

    drop(lock);
    let _next = duded::lock::StateLock::take(&state).expect("a racing start acquires the lock");
    std::fs::write(&receipt, "complete the-old-run\n").expect("the old run completes");
    let stopped = stopped
        .recv_timeout(Duration::from_secs(10))
        .expect("the old run completes while the new run holds its own lock");
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
    assert_eq!(stopped.stdout, "the daemon stopped\n");
}

#[test]
fn stop_does_not_claim_completion_from_an_old_empty_acknowledgment() {
    let dir = tempfile::tempdir().expect("a daemon's directories");
    let elsewhere = tempfile::tempdir().expect("a state directory the daemon never used");
    let socket = answer_one_shutdown(dir.path(), serde_json::json!({}));

    let run = Cli::new(&socket, dir.path())
        .env("DUDES_STATE_DIR", elsewhere.path())
        .args(["stop", "--hard"])
        .run();
    assert_eq!(run.code(), exit::OTHER, "{}", run.output());
    assert!(
        run.stderr.contains("does not report completion"),
        "{}",
        run.output()
    );
    assert!(run.stdout.is_empty(), "{}", run.output());
}

#[test]
fn stop_requires_a_positive_receipt_for_the_exact_answering_run() {
    let dir = tempfile::tempdir().expect("runtime");
    let receipt = dir.path().join("completion");
    std::fs::write(&receipt, "complete another-run\n").expect("stale receipt");
    let socket = answer_one_shutdown(
        dir.path(),
        serde_json::json!({
            "completion": {"path": receipt, "run": "this-run"}
        }),
    );
    let stopped = stop_in_background(Cli::new(&socket, dir.path()));
    assert!(stopped.recv_timeout(Duration::from_millis(500)).is_err());
    std::fs::remove_file(&receipt).expect("missing is not success");
    assert!(stopped.recv_timeout(Duration::from_millis(500)).is_err());
    std::fs::write(&receipt, "complete this-run\n").expect("positive exact receipt");
    let stopped = stopped
        .recv_timeout(Duration::from_secs(10))
        .expect("completed");
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
}

#[test]
fn stop_reports_a_known_teardown_failure() {
    let dir = tempfile::tempdir().expect("runtime");
    let receipt = dir.path().join("completion");
    std::fs::write(&receipt, "failed this-run managed child cleanup\n").expect("failed receipt");
    let socket = answer_one_shutdown(
        dir.path(),
        serde_json::json!({
            "completion": {"path": receipt, "run": "this-run"}
        }),
    );
    let stopped = Cli::new(&socket, dir.path()).args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), exit::OTHER, "{}", stopped.output());
    assert!(
        stopped.stderr.contains("managed child cleanup"),
        "{}",
        stopped.output()
    );
    assert!(stopped.stdout.is_empty());
}

/// After any managed turn the native host proves only that every agent process exited, not that
/// nothing escaped its process group or job. That stop succeeds, and says so.
#[test]
fn stop_warns_and_succeeds_when_only_child_containment_is_unproven() {
    let dir = tempfile::tempdir().expect("runtime");
    let receipt = dir.path().join("completion");
    std::fs::write(
        &receipt,
        "failed this-run managed child containment unproven\n",
    )
    .expect("receipt");
    let socket = answer_one_shutdown(
        dir.path(),
        serde_json::json!({
            "completion": {"path": receipt, "run": "this-run"}
        }),
    );
    let stopped = Cli::new(&socket, dir.path()).args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
    assert_eq!(stopped.stdout, "the daemon stopped\n");
    assert_eq!(stopped.stderr, CONTAINMENT_WARNING);
}

#[test]
fn unproven_containment_beside_another_teardown_failure_still_fails() {
    let dir = tempfile::tempdir().expect("runtime");
    let receipt = dir.path().join("completion");
    std::fs::write(
        &receipt,
        "failed this-run managed child containment unproven, Floor listener\n",
    )
    .expect("receipt");
    let socket = answer_one_shutdown(
        dir.path(),
        serde_json::json!({
            "completion": {"path": receipt, "run": "this-run"}
        }),
    );
    let stopped = Cli::new(&socket, dir.path()).args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), exit::OTHER, "{}", stopped.output());
    assert!(
        stopped.stderr.contains("Floor listener"),
        "{}",
        stopped.output()
    );
    assert!(stopped.stdout.is_empty());
}

#[test]
fn the_daemon_reports_unproven_containment_as_a_completed_stop() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::unproven_containment_shutdown(),
        Arc::new(OpenRunner::default()),
    );
    let stopped = Cli::new(&settings.socket(), &settings.runtime_dir)
        .args(["stop", "--hard"])
        .run();
    assert_eq!(stopped.code(), exit::SUCCESS, "{}", stopped.output());
    assert_eq!(stopped.stdout, "the daemon stopped\n");
    assert_eq!(stopped.stderr, CONTAINMENT_WARNING);
    let _next = duded::lock::StateLock::take(&settings.state_dir)
        .expect("the stopped daemon released the state lock");
}

#[test]
fn the_daemon_reports_unconfirmed_child_cleanup_after_releasing_its_resources() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::failing_shutdown(),
        Arc::new(OpenRunner::default()),
    );
    let stopped = Cli::new(&settings.socket(), &settings.runtime_dir)
        .args(["stop", "--hard"])
        .run();
    assert_eq!(stopped.code(), exit::OTHER, "{}", stopped.output());
    assert!(
        stopped.stderr.contains("managed child cleanup"),
        "{}",
        stopped.output()
    );
    assert!(stopped.stdout.is_empty());
    let _next = duded::lock::StateLock::take(&settings.state_dir)
        .expect("later teardown released the state lock");
}

#[test]
fn a_queued_call_that_cannot_be_journaled_prevents_a_clean_receipt() {
    use dudes_proto::rpc::system::ShutdownResult;
    use dudes_proto::rpc::{ErrorCode, Method};

    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let hired: dudes_proto::rpc::agent::HireResult =
        Cli::new(&settings.socket(), &settings.runtime_dir)
            .arg("--json")
            .args(common::hire_args(fixture.state.path()))
            .run()
            .json();
    running.stop();
    let daemon = duded::boot(
        &settings,
        common::seams(SharedHost::new(), Arc::new(OpenRunner::default())),
    )
    .expect("restart over the hired agent");
    let handle = daemon.handle();
    handle.stop(duded::ExitCause::ShutdownRpc);
    let mut client = dudes_rpc::LocalClient::connect(daemon.socket()).expect("shutdown connection");
    let credential = dudes::credential::read_operator_credential(&settings.runtime_dir)
        .expect("read credential")
        .expect("daemon credential");
    let response = client
        .call_as_operator(Method::SystemShutdown, None, credential)
        .expect("acknowledgment");
    let result: ShutdownResult =
        serde_json::from_value(response.result.expect("result")).expect("shutdown result");
    let receipt = result.completion.expect("run receipt");
    let answer = handle
        .ask_queued(duded::ports::Ask::Pause(hired.agent))
        .expect("queued pause");
    daemon
        .append_failure()
        .fail_nth(std::num::NonZeroUsize::new(1).expect("one append"));
    daemon.run().expect("the teardown runs");
    let refused = answer
        .recv_timeout(Duration::from_secs(5))
        .expect("pause answer")
        .expect_err("the pause cannot be journaled");
    assert_eq!(refused.code, ErrorCode::InternalError.code());
    assert_eq!(
        std::fs::read_to_string(receipt.path).expect("terminal receipt"),
        format!("failed {} journal drain\n", receipt.run)
    );
    let _next = duded::lock::StateLock::take(&settings.state_dir)
        .expect("the failed drain still releases the state lock");
}

/// A stand-in daemon on `dir/duded.sock` that answers one `system.shutdown` and then closes, as a
/// real one does before it finishes exiting.
fn answer_one_shutdown(dir: &Path, result: serde_json::Value) -> std::path::PathBuf {
    let socket = dir.join("duded.sock");
    let listener = UnixListener::bind(&socket).expect("a stand-in daemon");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the CLI connects");
        let mut request = String::new();
        BufReader::new(&stream)
            .read_line(&mut request)
            .expect("the CLI's request");
        let request: serde_json::Value = serde_json::from_str(&request).expect("a JSON request");
        assert_eq!(request["method"], "system.shutdown", "{request}");
        let answer = serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
        writeln!(&stream, "{answer}").expect("the answer");
    });
    socket
}

fn stop_in_background(cli: Cli) -> mpsc::Receiver<Run> {
    let (sender, stopped) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(cli.args(["stop", "--hard"]).run());
    });
    stopped
}
