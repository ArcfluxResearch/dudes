//! The pipe on an agent child's standard output: that it is read, that reading it is what keeps
//! the child from blocking, and that the child is reaped and its exit reported.
//!
//! # The two arms, and why the second is the one that matters
//!
//! Reading a short stream proves the wiring exists. It does **not** prove the wiring is safe: a
//! host that piped stdout and read it only after `wait` returned would pass that arm and deadlock
//! on any child that wrote more than one pipe buffer — the child blocked in `write(2)`, the host
//! blocked in `wait`, neither able to move. `the_child_is_not_blocked_by_a_full_pipe` is the arm
//! that would catch it, and it is why the helper writes [`MORE_THAN_A_PIPE_HOLDS`] bytes before
//! its `result` line.
//!
//! Both arms assert the *same* end — the session, the last message, the exit — so the second is
//! not merely "it finished": it is "it finished with everything the short one had", which is what
//! rules out a host that survived the flood by dropping the pipe.
//!
//! # Why the assertions are on the sink and not on a journal
//!
//! This crate has no journal and no supervisor. What it owes is lines and an exit; what a line
//! *means* belongs to `dudes-providers`, and what a turn's end means belongs to the daemon —
//! `bins/duded/tests/managed_turn.rs` is where the three meet.

mod common;

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use common::{MORE_THAN_A_PIPE_HOLDS, STREAM_SESSION, duplex_spec, no_install, stream_spec};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_supervisor::host::{AdoptionKey, KillOutcome, Liveness, ProcessHost};

/// The helper entry point every process test in this crate needs; see `common`.
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

/// How long a test waits for a child that writes a megabyte and exits. Generous, because the point
/// of the wait is to fail rather than hang when the child is blocked on a full pipe.
const PATIENCE: Duration = Duration::from_secs(30);

/// What one child's stream ended as.
struct Ended {
    lines: Vec<String>,
    exit: ChildExit,
}

/// A sink that keeps every line and announces the end on a channel.
struct Collect {
    ended: Mutex<Sender<Ended>>,
}

impl Collect {
    fn new() -> (Arc<Self>, Receiver<Ended>) {
        let (tx, rx) = channel();
        (
            Arc::new(Self {
                ended: Mutex::new(tx),
            }),
            rx,
        )
    }
}

impl ChildStreams for Collect {
    fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(One {
            lines: Vec::new(),
            ended: self
                .ended
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        })
    }
}

struct One {
    lines: Vec<String>,
    ended: Sender<Ended>,
}

impl ChildStream for One {
    fn line(&mut self, text: &str) {
        self.lines.push(text.to_owned());
    }

    fn ended(self: Box<Self>, exit: ChildExit) {
        let _ = self.ended.send(Ended {
            lines: self.lines,
            exit,
        });
    }
}

/// The lines of a stream that are JSON, in order.
///
/// The helper is this test binary re-executed, so the test harness prints a line of its own on the
/// child's standard output before the role takes over. That is not noise to be worked around: it
/// is the shape a real stream can have, where a vendor prints a warning beside its JSON, and it is
/// why `TurnStream` counts a line it cannot read rather than refusing the turn. The assertions
/// here are therefore about *which* JSON lines arrived and in what order, never about an index
/// into everything the child wrote.
fn json_lines(ended: &Ended) -> Vec<&str> {
    ended
        .lines
        .iter()
        .map(String::as_str)
        .filter(|line| line.starts_with('{'))
        .collect()
}

/// Run the `stream` helper under a watching host with `flood` bytes of filler, and return what the
/// sink was told.
fn run_stream_turn(flood: usize) -> Ended {
    let root = tempfile::TempDir::new().expect("a temporary directory");
    let mark = root.path().join("stream.pid");
    let (sink, ends) = Collect::new();
    let mut host = RealHost::watching(sink);

    common::spawned(
        host.spawn(&stream_spec(&mark, root.path(), flood), &no_install()),
        "the helper launches and its key is read",
    );
    let ended = ends
        .recv_timeout(PATIENCE)
        .expect("the child's stream ended and it was reaped");

    // The child has already been reaped by its own watcher, so this asserts the *other* half of
    // the restructure: a shutdown over an already-reaped child returns rather than waiting for a
    // message that will never come.
    common::shutdown_retaining_incomplete(&mut host, "shutdown over a reaped child");
    ended
}

/// Run the duplex helper with `stdin_len` bytes and return the complete stream its sink saw.
fn run_duplex_turn(stdin_len: usize) -> Ended {
    let root = tempfile::TempDir::new().expect("a temporary directory");
    let mark = root.path().join("duplex.pid");
    let (sink, ends) = Collect::new();
    let mut host = RealHost::watching(sink);
    let stdin = vec![b'i'; stdin_len];

    common::spawned(
        host.spawn(&duplex_spec(&mark, root.path(), stdin), &no_install()),
        "the duplex helper launches and all standard input is delivered",
    );
    let ended = ends
        .recv_timeout(PATIENCE)
        .expect("the duplex helper's stream ended and it was reaped");
    common::shutdown_retaining_incomplete(&mut host, "shutdown over a reaped child");
    ended
}

/// Assert the duplex helper received the exact input and never needed its bounded deadlock
/// release. The result is the helper's independent report, after all early output was drained.
fn assert_duplex_completed_without_release(ended: &Ended, stdin_len: usize) {
    let json = json_lines(ended);
    assert!(
        json.len() > MORE_THAN_A_PIPE_HOLDS / 4096,
        "all pipe-filling early output arrived: {} lines",
        json.len()
    );
    let result = json.last().expect("the duplex result line");
    assert!(
        result.contains(&format!(r#""stdin_bytes":{stdin_len}"#)),
        "the helper independently reports every stdin byte: {result}"
    );
    assert!(
        result.contains(r#""stdin_matches":true"#),
        "the helper independently reports the exact stdin payload: {result}"
    );
    assert!(
        result.contains(r#""release_fired":false"#),
        "the host broke the cycle before the helper's release: {result}"
    );
    assert_eq!((ended.exit.code, ended.exit.signal), (Some(0), None));
    assert!(ended.exit.elapsed_ms.is_some());
}

/// A small input fits in its pipe, so the existing host reaches its stdout watcher and completes.
/// This positive control separates the pipe-ordering deadlock from a broken helper or slow host.
#[test]
fn a_small_stdin_control_does_not_need_the_deadlock_release() {
    const SMALL_STDIN: usize = 17;
    let ended = run_duplex_turn(SMALL_STDIN);
    assert_duplex_completed_without_release(&ended, SMALL_STDIN);
}

/// Both directions exceed a pipe's capacity: the child fills stdout before reading stdin, while
/// the host must deliver all stdin. A serial implementation waits for the helper's ten-second
/// release; a duplex host drains stdout concurrently and never needs it.
#[test]
fn large_stdin_is_delivered_while_early_stdout_is_drained() {
    let stdin_len = MORE_THAN_A_PIPE_HOLDS + 4096;
    let ended = run_duplex_turn(stdin_len);
    assert_duplex_completed_without_release(&ended, stdin_len);
}

/// The `result` line and the `system/init` line reach the sink verbatim, in order, and the child's
/// exit is reported as the platform gave it.
///
/// The line count is asserted too: three lines and no more, so a reader that delivered the whole
/// stream as one line — or split it on something other than the newline — fails here rather than
/// passing on the two lines the assertions name.
#[test]
fn a_managed_childs_stdout_reaches_the_sink() {
    let ended = run_stream_turn(0);
    let json = json_lines(&ended);

    assert_eq!(json.len(), 2, "the init line and the result line: {json:?}");
    assert!(
        json[0].contains(STREAM_SESSION) && json[0].contains(r#""subtype":"init""#),
        "the first is the session announcement: {:?}",
        json[0]
    );
    assert!(
        json[1].contains(r#""type":"result""#) && json[1].contains("the task is done"),
        "the second is the result: {:?}",
        json[1]
    );
    assert_eq!((ended.exit.code, ended.exit.signal), (Some(0), None));
    assert!(ended.exit.elapsed_ms.is_some());
}

/// The hazard the pipe was not opened for until now: a child that writes more than a pipe buffer
/// blocks in `write(2)` unless somebody is draining it *while* it writes.
///
/// Red against a host that pipes and reads only after the wait: the child fills the buffer, the
/// host never reaps, and this fails on [`PATIENCE`] rather than on an assertion. Green here, with
/// the same end as the short stream — the `result` line arrives, the session is intact and the
/// exit is 0 — so a host that "survived" by dropping the pipe fails too.
#[test]
fn the_child_is_not_blocked_by_a_full_pipe() {
    let ended = run_stream_turn(MORE_THAN_A_PIPE_HOLDS);
    let json = json_lines(&ended);

    assert!(
        json.len() > MORE_THAN_A_PIPE_HOLDS / 4096,
        "every filler line arrived, not just the ones that fit in one buffer: {} lines",
        json.len()
    );
    let last = json.last().expect("a result line");
    assert!(
        last.contains(r#""type":"result""#) && last.contains("the task is done"),
        "the result line is the last one and it survived the flood: {last:?}"
    );
    assert!(
        json[0].contains(STREAM_SESSION),
        "and the first one did too: {:?}",
        json[0]
    );
    assert_eq!((ended.exit.code, ended.exit.signal), (Some(0), None));
    assert!(ended.exit.elapsed_ms.is_some());
}

/// A host with no sink opens no pipe at all, and one with a sink does — asserted on the child's
/// own descriptor rather than on this crate's behaviour.
///
/// # Why the assertion is an identity and not a shape
///
/// "Is my standard output a pipe?" cannot answer this: under `cargo nextest` the runner's own
/// stdout is already a pipe, so an inherited descriptor and a fresh one would both say yes. What
/// separates them is *which* descriptor: an inherited stdout is the very same open file
/// description this process holds, so `fstat` reports the same device and inode; a pipe the host
/// created reports a different one. The helper reports what it sees and the test compares it with
/// its own — the child is the witness, not the code under test.
///
/// # What this is the control on, and how it fails when it fails
///
/// A one-sided version would assert only `owned() == 1`, `shutdown()`,
/// `owned() == 0` — ownership and reaping, nothing about stdout — and a host that piped
/// **unconditionally**, with no sink and nobody reading, would pass it and every other arm of this
/// file. The claim it exists to control ("the pipe and its reader are created together") would
/// rest on reading the code.
///
/// This test does not rest on that, and the way that mutation fails here is worth stating rather than guessing at:
/// with no consumer, `read_in_background` drops the pipe, so the helper takes `EPIPE` on its very
/// first write — which under this harness is the test runner's own banner, before the role even
/// starts. The child dies without reporting anything and this fails on the wait, naming the report
/// that never arrived (`error: io error when listing tests: Broken pipe` beside it). So the red is
/// a timeout in form and a killed child in substance; the sink arm below, which passes in the same
/// run, is what makes it attributable to the mutation rather than to a slow machine.
#[cfg(unix)]
#[test]
fn a_host_with_no_sink_opens_no_pipe() {
    let root = tempfile::TempDir::new().expect("a temporary directory");
    let mine = common::stdout_identity();

    // No sink: the child's stdout is this process's own.
    let mark = root.path().join("inherited.pid");
    let mut host = RealHost::new();
    common::spawned(
        host.spawn(&stream_spec(&mark, root.path(), 0), &no_install()),
        "the helper launches",
    );
    assert_eq!(host.owned(), 1, "the host owns it either way");
    assert_eq!(
        common::await_stdout_identity(&mark),
        mine,
        "with no sink the child inherits this process's standard output"
    );
    common::shutdown_retaining_incomplete(&mut host, "the child is reaped at shutdown");

    // A sink: the child's stdout is a pipe the host made, so it is a different description.
    let mark = root.path().join("piped.pid");
    let (sink, ends) = Collect::new();
    let mut host = RealHost::watching(sink);
    common::spawned(
        host.spawn(&stream_spec(&mark, root.path(), 0), &no_install()),
        "the helper launches",
    );
    let reported = common::await_stdout_identity(&mark);
    ends.recv_timeout(PATIENCE).expect("the stream ended");
    common::shutdown_retaining_incomplete(&mut host, "shutdown over a reaped child");
    assert_ne!(
        reported, mine,
        "with a sink the child writes into a pipe, not into this process's standard output"
    );
}

/// A kill for a child its own watcher has already reaped is **refused**, not signalled.
///
/// This is the state the watcher thread creates and the one `guard_for` exists for: the child exits on its
/// own, the watcher reaps it, and an `Effect::Kill` decided before the supervisor folded
/// `ChildExited` arrives afterwards.
///
/// The identifier has been released by then, so the reservation no longer authorises anything and
/// the key comparison must decide. Unix observes `Gone`; Windows conservatively reports `Unknown`
/// for a terminated or unresolvable process. Both withhold the signal. Forcing `Guard::Reserved`
/// makes this signal an identifier the host no longer owns and answer `Signalled`, which is the
/// hazard measured by this assertion.
#[test]
fn killing_a_child_its_watcher_already_reaped_is_refused() {
    let root = tempfile::TempDir::new().expect("a temporary directory");
    let mark = root.path().join("stream.pid");
    let (sink, ends) = Collect::new();
    let mut host = RealHost::watching(sink);

    let key = common::spawned(
        host.spawn(&stream_spec(&mark, root.path(), 0), &no_install()),
        "the helper launches and its key is read",
    );
    // `ended` is published after the reap, so past this line `reaped()` is true by construction.
    ends.recv_timeout(PATIENCE)
        .expect("the child's stream ended and it was reaped");

    let outcome = host.kill(&key);
    #[cfg(windows)]
    let expected = Liveness::Unknown;
    #[cfg(not(windows))]
    let expected = Liveness::Gone;
    assert!(
        matches!(
            outcome,
            KillOutcome::Refused { liveness } if liveness == expected
        ),
        "a released identifier is compared, not signalled: {outcome:?}"
    );
    common::shutdown_retaining_incomplete(&mut host, "shutdown after a refused kill");
}
