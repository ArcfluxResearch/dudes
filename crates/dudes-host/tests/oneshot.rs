//! The three predicate-child obligations, against real processes.
//!
//! Obligation 1 (the platform hold at spawn) is not asserted directly here — it has no observable
//! of its own — but it is what obligations 2 and 3 are *made of*: the descendant arm below only
//! passes if the child led a group of its own, because a signal to a child that inherited this
//! process's group would either reach nothing or reach the test runner.
//!
//! **Unix only, and the reason is the assertions rather than the code.** `OneShot` compiles and
//! runs on all three platforms; what is Unix-shaped is the *evidence* — a descendant that is
//! reachable only by a group sweep is the Unix hold's failure mode, and on Windows the job object's
//! `KILL_ON_JOB_CLOSE` makes obligation 3 defence in depth behind obligation 1 rather than the load-
//! bearing thing. The two `/bin`-resolved programs below are the other half.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_providers::launch::LaunchSpec;

mod common;

/// A grace short enough that a test does not pay `DEFAULT_GRACE` for every kill, and long enough
/// that a child which does exit on `SIGTERM` is not escalated to `SIGKILL` for being slow.
const GRACE: Duration = Duration::from_millis(200);

/// Longer than any child here needs, so a run that ends is a run the child ended.
const GENEROUS: Duration = Duration::from_secs(30);

/// The Linux control measured a 200 ms beat interval under two-core contention. 800 ms keeps
/// four times that observed delay while still failing promptly when a helper never beats, and keeps
/// the timeout test inside the five-second test limit.
const LIVE_CONTROL: Duration = Duration::from_millis(800);

/// This bound starts before the two serial liveness controls. It must outlive both controls or the
/// test can kill the grandchild while it is still trying to prove that the grandchild is live.
const OUTLIVES_CONTROLS: Duration = Duration::from_millis(2_500);

/// The one `#[test]` that lets this binary be re-executed as a helper. See `common`.
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

#[test]
fn protocol_input_is_refused_before_a_one_shot_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let mark = dir.path().join("protocol.pid");
    let mut spec = common::stream_spec(&mark, dir.path(), 0);
    spec.stdin = dudes_providers::launch::LaunchInput::Protocol(
        dudes_providers::codex::startup::CodexStartup::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "task".into(),
            dudes_providers::adapter::TurnSession::Assigned,
            dudes_providers::adapter::Adapter::model_selection(
                &dudes_providers::codex::managed::CodexAdapter::default(),
            ),
        ),
    );
    let error = OneShot::with_grace(GRACE).run(&spec, GENEROUS).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!mark.exists(), "a refused protocol launch ran helper code");
}

/// The first of `candidates` that exists, as a program to run.
///
/// Absolute and resolved here rather than left to a `PATH` lookup, because `OneShot` is handed a
/// spec whose `argv[0]` the *daemon* has already resolved and canonicalized —
/// there is no lookup inside the runner to lean on.
fn program(candidates: &[&str]) -> PathBuf {
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }
    panic!("none of {candidates:?} exists on this platform");
}

/// A spec that runs `argv` in `cwd` with an empty environment.
fn spec(argv: &[&std::ffi::OsStr], cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        argv: argv.iter().map(|part| (*part).to_owned()).collect(),
        env: Vec::new(),
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    }
}

/// Obligation 2's ordinary case: a command that ends on its own is reaped, its exit code is the
/// evidence, and its output is captured.
///
/// The zero arm is the control for the nonzero one: without it a runner that reported `Some(1)`
/// for everything would pass, and `exit` is what `Evidence::Command` carries and what a predicate
/// is judged on.
#[test]
fn a_command_that_ends_on_its_own_reports_its_code_and_its_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let echo = program(&["/bin/echo", "/usr/bin/echo"]);

    let marker = "dudes-oneshot-marker";
    let outcome = runner
        .run(
            &spec(&[echo.as_os_str(), marker.as_ref()], dir.path()),
            GENEROUS,
        )
        .expect("echo runs");
    match outcome {
        OneShotOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        } => {
            assert_eq!(exit, Some(0), "control: a successful command exits 0");
            assert!(!timed_out, "it ended well inside its timeout");
            assert!(
                output_tail.contains(marker),
                "the tail carries what the child printed: {output_tail:?}"
            );
        }
        OneShotOutcome::Swept => panic!("nothing swept this run"),
    }

    let no = program(&["/usr/bin/false", "/bin/false"]);
    let outcome = runner
        .run(&spec(&[no.as_os_str()], dir.path()), GENEROUS)
        .expect("false runs");
    assert!(
        matches!(outcome, OneShotOutcome::Finished { exit: Some(code), .. } if code != 0),
        "a failing command reports its nonzero code: {outcome:?}"
    );

    assert_eq!(
        runner.in_flight(),
        0,
        "run returns only after the child is reaped and deregistered"
    );
}

/// A discarding run hands the child the null device for both output streams and reads nothing.
///
/// `test -c /dev/fd/N` exits 0 only when that stream is a character device, which the null device
/// is and the capture pipe of [`OneShot::run`] is not. The same argv through `run` is the control:
/// it exits 1, so the zero is the null device and not a `test` that always passes.
#[test]
fn a_discarding_run_gives_the_child_the_null_device_and_reads_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let test = program(&["/bin/test", "/usr/bin/test"]);
    for stream in ["/dev/fd/1", "/dev/fd/2"] {
        let argv = [test.as_os_str(), "-c".as_ref(), stream.as_ref()];
        let discarded = runner
            .run_discarding_output(&spec(&argv, dir.path()), GENEROUS)
            .expect("test runs");
        assert!(
            matches!(
                &discarded,
                OneShotOutcome::Finished { exit: Some(0), output_tail, timed_out: false }
                    if output_tail.is_empty()
            ),
            "{stream} is the null device and nothing was read: {discarded:?}"
        );
        let captured = runner
            .run(&spec(&argv, dir.path()), GENEROUS)
            .expect("test runs");
        assert!(
            matches!(captured, OneShotOutcome::Finished { exit: Some(1), .. }),
            "control: under run, {stream} is a pipe: {captured:?}"
        );
    }

    let echo = program(&["/bin/echo", "/usr/bin/echo"]);
    let outcome = runner
        .run_discarding_output(
            &spec(&[echo.as_os_str(), "dudes-discarded".as_ref()], dir.path()),
            GENEROUS,
        )
        .expect("echo runs");
    assert!(
        matches!(&outcome, OneShotOutcome::Finished { exit: Some(0), output_tail, .. } if output_tail.is_empty()),
        "a child that prints still yields no tail: {outcome:?}"
    );
    assert_eq!(runner.in_flight(), 0, "and it was reaped and deregistered");
}

/// Obligation 2's hard case: a child that outlives `timeout_s` is killed and reaped **before**
/// `run` returns, and its descendants go with it.
///
/// The control is the anti-vacuity one this crate uses everywhere: the child is asserted to be
/// *beating* while the run is in flight, so a later "not beating" is this call's doing and not a
/// helper that never started. The grandchild is what makes it a statement about the group: it
/// inherits the helper's group deliberately, so a signal to the helper's identifier alone never
/// reaches it.
#[test]
fn a_child_that_outlives_its_timeout_is_killed_with_its_group_and_reaped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("child.pid");
    let grandchild = dir.path().join("grandchild.pid");
    let runner = Arc::new(OneShot::with_grace(GRACE));

    let spec = common::helper_spec(common::GRANDCHILD, &mark, dir.path());
    let handle = {
        let runner = Arc::clone(&runner);
        thread::spawn(move || runner.run(&spec, OUTLIVES_CONTROLS))
    };

    common::await_number(&mark);
    common::await_number(&grandchild);
    assert!(
        common::beating_within(&common::beat_path(&mark), LIVE_CONTROL),
        "control: the child is running, so a later silence is the kill"
    );
    assert!(
        common::beating_within(&common::beat_path(&grandchild), LIVE_CONTROL),
        "control: the grandchild is running too"
    );

    let outcome = handle.join().expect("the run thread").expect("the run");
    assert!(
        matches!(
            outcome,
            OneShotOutcome::Finished {
                timed_out: true,
                ..
            }
        ),
        "the run hit its timeout: {outcome:?}"
    );
    assert!(
        !common::beating(&common::beat_path(&mark)),
        "the child is gone before run returned"
    );
    assert!(
        !common::beating_within(&common::beat_path(&grandchild), LIVE_CONTROL),
        "the group went with it: a descendant only a group sweep reaches is gone too"
    );
    assert_eq!(runner.in_flight(), 0, "and it was deregistered");
}

/// Obligation 3, at this crate's level: an in-flight predicate is reachable from another thread,
/// and the sweep that reaches it produces an outcome carrying **no evidence**.
///
/// `Swept` is the assertion that matters. A struct with a `swept: bool` would let a caller build a
/// `Record::PredicateResult` out of an interrupted run and journal a fabricated
/// `Provenance::Observed` verdict, which invariant 3 forbids; the enum makes that unrepresentable,
/// and this is where that is exercised rather than only stated.
#[test]
fn a_sweep_ends_an_in_flight_predicate_and_yields_no_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("child.pid");
    let runner = Arc::new(OneShot::with_grace(GRACE));

    let spec = common::helper_spec(common::PARK, &mark, dir.path());
    let handle = {
        let runner = Arc::clone(&runner);
        // A timeout far longer than this test: nothing here may end because the bound expired.
        thread::spawn(move || runner.run(&spec, GENEROUS))
    };

    common::await_number(&mark);
    assert!(
        common::beating_within(&common::beat_path(&mark), LIVE_CONTROL),
        "control: the predicate is running when the sweep is called"
    );
    assert_eq!(
        runner.in_flight(),
        1,
        "control: the exit path can see it, which is what obligation 3 is"
    );

    // The exit path, on the thread the daemon exits on, through the same `&self` it holds.
    runner.sweep();

    let outcome = handle.join().expect("the run thread").expect("the run");
    assert!(
        matches!(outcome, OneShotOutcome::Swept),
        "a swept run carries no evidence: {outcome:?}"
    );
    assert!(
        !common::beating(&common::beat_path(&mark)),
        "the swept child is gone"
    );
    assert_eq!(runner.in_flight(), 0);
}

/// A cancelled run ends its own child well before its timeout, reaps it, and yields no evidence,
/// while the runner stays open for the next run.
///
/// Mutation: ignoring the flag leaves the child beating until the generous timeout.
#[test]
fn a_cancelled_run_ends_its_child_and_yields_no_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("child.pid");
    let runner = Arc::new(OneShot::with_grace(GRACE));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let spec = common::helper_spec(common::PARK, &mark, dir.path());
    let handle = {
        let (runner, cancel) = (Arc::clone(&runner), Arc::clone(&cancel));
        thread::spawn(move || runner.run_cancellable(&spec, GENEROUS, &cancel))
    };

    common::await_number(&mark);
    assert!(
        common::beating_within(&common::beat_path(&mark), LIVE_CONTROL),
        "control: the predicate is running when the run is cancelled"
    );
    cancel.store(true, std::sync::atomic::Ordering::Release);

    let outcome = handle.join().expect("the run thread").expect("the run");
    assert!(
        matches!(outcome, OneShotOutcome::Swept),
        "a cancelled run carries no evidence: {outcome:?}"
    );
    assert!(
        !common::beating(&common::beat_path(&mark)),
        "the cancelled child is gone"
    );
    assert_eq!(runner.in_flight(), 0);
    let probe = dir.path().join("after");
    let after = runner
        .run(&spec_after(&probe, dir.path()), GENEROUS)
        .expect("a later run");
    assert!(
        matches!(after, OneShotOutcome::Finished { exit: Some(0), .. }),
        "the runner is still open: {after:?}"
    );
}

fn spec_after(touch: &Path, cwd: &Path) -> LaunchSpec {
    let program = program(&["/usr/bin/touch", "/bin/touch"]);
    spec(&[program.as_os_str(), touch.as_os_str()], cwd)
}

/// A run that starts after the sweep starts no child and yields no evidence, through both run
/// methods.
///
/// The exit path's sweep reaches only the children registered when it runs, and a predicate thread
/// the daemon started just before it can reach the runner just after. A child started then would
/// sit in its own group with nothing left to sweep it and outlive the daemon. The open runner is
/// the control: the same spec does start a child there, so the absent marker is the sweep's doing.
#[test]
fn a_run_that_starts_after_the_sweep_starts_no_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("started");
    let sh = program(&["/bin/sh"]);
    let script = format!("echo started > '{}'", mark.display());
    let spec = spec(
        &[sh.as_os_str(), "-c".as_ref(), script.as_ref()],
        dir.path(),
    );

    let open = OneShot::with_grace(GRACE);
    let outcome = open.run(&spec, GENEROUS).expect("the open runner runs it");
    assert!(
        matches!(outcome, OneShotOutcome::Finished { exit: Some(0), .. }),
        "control: an open runner starts the child: {outcome:?}"
    );
    assert!(mark.exists(), "control: the child wrote its marker");
    std::fs::remove_file(&mark).expect("reset the marker");

    let swept = OneShot::with_grace(GRACE);
    swept.sweep();
    let outcome = swept.run(&spec, GENEROUS).expect("a late run answers");
    assert!(
        matches!(outcome, OneShotOutcome::Swept),
        "a run after the sweep yields no evidence: {outcome:?}"
    );
    let outcome = swept
        .run_discarding_output(&spec, GENEROUS)
        .expect("a late discarding run answers");
    assert!(
        matches!(outcome, OneShotOutcome::Swept),
        "a discarding run after the sweep yields no evidence: {outcome:?}"
    );
    assert!(!mark.exists(), "no child started after the sweep");
    assert_eq!(swept.in_flight(), 0);
}

/// A sweep with nothing in flight is a no-op, and a second sweep after one is too.
///
/// Idempotence is part of the obligation, and the exit path calls this on a daemon that may never have
/// run a predicate at all.
#[test]
fn a_sweep_with_nothing_in_flight_is_a_no_op() {
    let runner = OneShot::with_grace(GRACE);
    runner.sweep();
    runner.sweep();
    assert_eq!(runner.in_flight(), 0);
}

/// The seam's shape, as a compile-time assertion: `&self`, `Send + Sync`, reachable through an
/// `Arc` from a thread that is not the one running the predicate.
///
/// This is the arm that fails to compile if `run` or `sweep` ever takes `&mut self` — which is the
/// change that would reproduce, on the exit path, exactly the stall that obligation 3 rules out for `RealHost`
/// for.
#[test]
fn the_runner_is_shareable_between_the_predicate_thread_and_the_exit_path() {
    fn exit_path(runner: &Arc<OneShot>) {
        runner.sweep();
    }
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OneShot>();

    let runner = Arc::new(OneShot::with_grace(GRACE));
    let other = Arc::clone(&runner);
    let handle = thread::spawn(move || exit_path(&other));
    exit_path(&runner);
    handle.join().expect("the other thread");
}

fn captured(
    runner: &OneShot,
    argv: &[&std::ffi::OsStr],
    cwd: &Path,
    timeout: Duration,
    cap: usize,
) -> CapturedOutcome {
    runner
        .run_captured(&spec(argv, cwd), timeout, cap)
        .expect("the captured run answers")
}

/// A captured run keeps standard output and drops standard error. The stderr line is the control
/// for the first claim: a runner that merged the streams would put it in `stdout`.
#[test]
fn a_captured_run_returns_stdout_and_never_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let sh = program(&["/bin/sh"]);
    let argv = [
        sh.as_os_str(),
        "-c".as_ref(),
        "printf out; printf err >&2".as_ref(),
    ];
    let CapturedOutcome::Finished {
        exit,
        stdout,
        truncated,
        timed_out,
    } = captured(&runner, &argv, dir.path(), GENEROUS, 1024)
    else {
        panic!("nothing swept this run");
    };
    assert_eq!(
        (exit, stdout.as_slice(), truncated, timed_out),
        (Some(0), b"out".as_slice(), false, false)
    );
    assert_eq!(runner.in_flight(), 0);
}

/// A child that prints without end is killed at the cap and costs the cap, not the timeout. The
/// timeout here is thirty seconds, so a run that ends in a few is a run the cap ended.
#[test]
fn a_captured_run_that_overflows_its_cap_is_killed_at_the_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let yes = program(&["/usr/bin/yes", "/bin/yes"]);
    let started = std::time::Instant::now();
    let CapturedOutcome::Finished {
        exit,
        stdout,
        truncated,
        timed_out,
    } = captured(&runner, &[yes.as_os_str()], dir.path(), GENEROUS, 1000)
    else {
        panic!("nothing swept this run");
    };
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cap ended it"
    );
    assert_eq!(stdout.len(), 1000, "exactly the cap was kept");
    assert!(
        stdout.starts_with(b"y\ny\n"),
        "from the start of the output"
    );
    assert!(
        truncated && !timed_out && exit.is_none(),
        "killed by the cap: {exit:?}"
    );
    assert_eq!(runner.in_flight(), 0);
}

/// Output of exactly the cap is not truncated: the boundary the overflow test cannot see.
#[test]
fn a_captured_run_of_exactly_the_cap_is_not_truncated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let sh = program(&["/bin/sh"]);
    let argv = [sh.as_os_str(), "-c".as_ref(), "printf abcd".as_ref()];
    let CapturedOutcome::Finished {
        stdout, truncated, ..
    } = captured(&runner, &argv, dir.path(), GENEROUS, 4)
    else {
        panic!("nothing swept this run");
    };
    assert_eq!((stdout.as_slice(), truncated), (b"abcd".as_slice(), false));
}

/// A captured child that outlives its timeout is killed and reported as timed out, with the output
/// it had already written.
#[test]
fn a_captured_run_that_outlives_its_timeout_is_killed_and_keeps_its_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let sh = program(&["/bin/sh"]);
    let argv = [
        sh.as_os_str(),
        "-c".as_ref(),
        "printf early; sleep 30".as_ref(),
    ];
    let CapturedOutcome::Finished {
        exit,
        stdout,
        truncated,
        timed_out,
    } = captured(&runner, &argv, dir.path(), Duration::from_millis(500), 1024)
    else {
        panic!("nothing swept this run");
    };
    assert_eq!(
        (stdout.as_slice(), truncated, timed_out, exit),
        (b"early".as_slice(), false, true, None)
    );
    assert_eq!(runner.in_flight(), 0);
}

/// A closed runner starts no captured child, as it starts no other.
#[test]
fn a_swept_runner_answers_swept_to_a_captured_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    runner.sweep();
    let echo = program(&["/bin/echo", "/usr/bin/echo"]);
    assert!(matches!(
        captured(&runner, &[echo.as_os_str()], dir.path(), GENEROUS, 16),
        CapturedOutcome::Swept
    ));
}

/// On Linux a captured child runs with the address-space limit, set after the spawn. `ulimit -v`
/// prints it in KiB. A plain run is the control: it has no limit.
#[cfg(target_os = "linux")]
#[test]
fn a_captured_child_has_the_address_space_limit_on_linux() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = OneShot::with_grace(GRACE);
    let sh = program(&["/bin/sh"]);
    // The limit lands just after the spawn, so the child asks once it has had time to be set.
    let argv = [
        sh.as_os_str(),
        "-c".as_ref(),
        "sleep 0.3; ulimit -v".as_ref(),
    ];
    let CapturedOutcome::Finished { stdout, .. } =
        captured(&runner, &argv, dir.path(), GENEROUS, 64)
    else {
        panic!("nothing swept this run");
    };
    assert_eq!(
        String::from_utf8_lossy(&stdout).trim(),
        (dudes_host::CAPTURED_ADDRESS_SPACE_BYTES / 1024).to_string()
    );
    let OneShotOutcome::Finished { output_tail, .. } = runner
        .run(&spec(&argv, dir.path()), GENEROUS)
        .expect("plain run")
    else {
        panic!("nothing swept this run");
    };
    assert_eq!(
        output_tail.trim(),
        "unlimited",
        "control: a plain run has no limit"
    );
}
