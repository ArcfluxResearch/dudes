//! Invariant 10, second half: the host's kill escalation has **landed by the time `shutdown`
//! returns**.
//!
//! Invariant 10 says every child dies *with* the
//! daemon, which is a claim about what has already happened at the moment the daemon stops — so the
//! escalation cannot be handed to a timer. The failure mode is specific and has shipped elsewhere:
//! a grace timer that does not hold the process open, armed on the quit path, whose sweep therefore
//! never runs because the process it was scheduled in has already exited.
//!
//! # What this test actually witnesses, which is narrower than "completes"
//!
//! Every stop reading is taken **after** `shutdown()` has returned its unresolved-ownership error,
//! and none waits for a condition: the children advance a counter every 20 ms while they run, and
//! the test reads those counters over a fixed window and requires that they do not move. So the
//! predicate is *"the children had stopped within roughly one heartbeat of `shutdown` returning"*,
//! and its resolution is that heartbeat. A deferral of 30 ms fails it; a deferral of zero — handing
//! the very same escalation to a thread and returning immediately — does not, because the thread
//! finishes inside the window. Measured, not assumed. Direct-child reap is
//! witnessed separately by keyed stream-end notices, through a bounded receive.
//!
//! That is written down rather than papered over because the alternative is a doc comment claiming
//! a stronger test than the one below it, which is worse than a narrow test: it stops anyone
//! looking for the stronger one. What closes the remaining 20 ms is not a better assertion here but
//! the shape of the code — `shutdown` contains no timer, no channel and no thread, and
//! `src/kill.rs`'s module docs are where that is argued.
//!
//! Three observations below are deliberately separate: `owned() == 3` proves incomplete rows were
//! retained, the counters prove every addressable process stopped, and the keyed non-default exit
//! notices prove the three direct-child waits succeeded. The elapsed bound says only that the grace
//! was spent once rather than per child.
//!
//! The grandchild is included because a sweep that only reaches direct children satisfies the
//! count and still leaks the tree.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_supervisor::host::{AdoptionKey, ContainmentUnproven, Liveness, ProcessHost};

#[derive(Clone, Copy, Debug)]
struct ReapNotice {
    pid: u32,
    exit: ChildExit,
}

struct ReapNotices {
    sender: Mutex<Sender<ReapNotice>>,
}

impl ReapNotices {
    fn new() -> (Arc<Self>, Receiver<ReapNotice>) {
        let (sender, receiver) = channel();
        (
            Arc::new(Self {
                sender: Mutex::new(sender),
            }),
            receiver,
        )
    }
}

impl ChildStreams for ReapNotices {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(ReapNoticeStream {
            pid: key
                .expect("a successful real-backend spawn must name its direct child")
                .pid,
            sender: self
                .sender
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        })
    }
}

struct ReapNoticeStream {
    pid: u32,
    sender: Sender<ReapNotice>,
}

impl ChildStream for ReapNoticeStream {
    fn line(&mut self, _text: &str) {}

    fn ended(self: Box<Self>, exit: ChildExit) {
        let _ = self.sender.send(ReapNotice {
            pid: self.pid,
            exit,
        });
    }
}

fn verify_reap_notices(
    receiver: &Receiver<ReapNotice>,
    expected: &BTreeSet<u32>,
    missing_bound: Duration,
    no_fourth_bound: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + missing_bound;
    let mut notices = Vec::with_capacity(expected.len());
    for _ in 0..expected.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(notice) => notices.push(notice),
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "missing direct-child reap notice: received {} of {}",
                    notices.len(),
                    expected.len()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "reap-notice channel disconnected after {} of {} direct children",
                    notices.len(),
                    expected.len()
                ));
            }
        }
    }

    match receiver.recv_timeout(no_fourth_bound) {
        Ok(notice) => {
            return Err(format!(
                "unexpected fourth direct-child reap notice for pid {}",
                notice.pid
            ));
        }
        Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => {
            return Err("reap-notice channel disconnected before the no-fourth bound".to_owned());
        }
    }

    if let Some(notice) = notices
        .iter()
        .find(|notice| notice.exit == ChildExit::default())
    {
        return Err(format!(
            "direct-child wait for pid {} returned no exit status",
            notice.pid
        ));
    }
    let seen_pids: BTreeSet<u32> = notices.iter().map(|notice| notice.pid).collect();
    if seen_pids.len() != notices.len() {
        return Err("duplicate direct-child reap notice".to_owned());
    }
    if &seen_pids != expected {
        return Err(format!(
            "direct-child reap notices did not match spawned pids: expected {expected:?}, received {seen_pids:?}"
        ));
    }
    Ok(())
}

/// The error a shutdown returns once every child stopped and was reaped, and it is one value rather
/// than a choice of two: only complete boundary coverage is missing.
///
/// A raw `EPERM` is not tolerated here as a group-state race: a delivered
/// group sweep produces no such answer — it is `SignalResult::NothingSignalled`, not a
/// platform error, so it never becomes shutdown's first error — and these helpers run as the host's
/// own uid, so no direct-leader `EPERM` is reachable either. Asserting the synthetic error instead
/// is what makes the shutdown half falsifiable: this reddens if a delivered `EPERM`
/// reaches `first_error` anyway.
#[cfg(unix)]
fn assert_containment_unproven(error: &std::io::Error) {
    assert_eq!(
        error.raw_os_error(),
        None,
        "a concrete platform error reached shutdown's first error: {error}"
    );
    assert!(ContainmentUnproven::is(error), "{error}");
}

#[cfg(unix)]
fn await_gone(host: &RealHost, key: &AdoptionKey) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while host.alive(key) != Liveness::Gone {
        assert!(
            Instant::now() < deadline,
            "direct leader {} was not observed gone after shutdown",
            key.pid
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn assert_retained_leader_stopped(host: &RealHost, key: &AdoptionKey) {
    assert_eq!(
        host.alive(key),
        Liveness::Unknown,
        "a terminated process behind retained handle {} is stopped but not absent",
        key.pid
    );
}

#[cfg(unix)]
fn assert_retained_leader_stopped(host: &RealHost, key: &AdoptionKey) {
    await_gone(host, key);
}

#[cfg(windows)]
fn assert_containment_unproven(error: &std::io::Error) {
    assert!(ContainmentUnproven::is(error), "{error}");
}

/// Not a test. See [`common::run_if_helper`].
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

fn successful_notice(pid: u32) -> ReapNotice {
    ReapNotice {
        pid,
        exit: ChildExit {
            elapsed_ms: None,
            code: Some(0),
            signal: None,
        },
    }
}

fn verify_fake_reap_notices(
    notices: &[ReapNotice],
    expected: &BTreeSet<u32>,
) -> Result<(), String> {
    const WAIT: Duration = Duration::from_millis(20);
    let (sender, receiver) = channel();
    for notice in notices {
        sender.send(*notice).expect("send fake notice");
    }
    let result = verify_reap_notices(&receiver, expected, WAIT, WAIT);
    drop(sender);
    result
}

#[test]
fn reap_notice_validation_rejects_false_evidence() {
    let expected = BTreeSet::from([11, 22, 33]);
    verify_fake_reap_notices(
        &[
            successful_notice(11),
            successful_notice(22),
            successful_notice(33),
        ],
        &expected,
    )
    .expect("the exact three successful direct-child notices are accepted");

    for (label, notices, error_fragment) in [
        (
            "default exit",
            vec![
                ReapNotice {
                    pid: 11,
                    exit: ChildExit::default(),
                },
                successful_notice(22),
                successful_notice(33),
            ],
            "returned no exit status",
        ),
        (
            "duplicate pid",
            vec![
                successful_notice(11),
                successful_notice(11),
                successful_notice(33),
            ],
            "duplicate direct-child reap notice",
        ),
        (
            "wrong pid",
            vec![
                successful_notice(11),
                successful_notice(22),
                successful_notice(44),
            ],
            "did not match spawned pids",
        ),
        (
            "suppressed notice",
            vec![successful_notice(11), successful_notice(22)],
            "missing direct-child reap notice",
        ),
        (
            "fourth notice",
            vec![
                successful_notice(11),
                successful_notice(22),
                successful_notice(33),
                successful_notice(44),
            ],
            "unexpected fourth direct-child reap notice",
        ),
    ] {
        let error = verify_fake_reap_notices(&notices, &expected).expect_err(label);
        assert!(error.contains(error_fragment), "{label}: {error}");
    }
}

/// Shutdown kills every child and every grandchild, and they have stopped by the time it returns.
fn verify_real_backend_shutdown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (reap_notices, reaped) = ReapNotices::new();
    let mut host = RealHost::watching_with_grace(reap_notices, Duration::from_millis(300));

    let mut beats: Vec<PathBuf> = Vec::new();
    let mut keys = Vec::new();
    for (index, role) in [common::PARK, common::PARK, common::GRANDCHILD]
        .iter()
        .enumerate()
    {
        let mark = dir.path().join(format!("child-{index}.pid"));
        let key = common::spawned(
            host.spawn(
                &common::helper_spec(role, &mark, dir.path()),
                &common::no_install(),
            ),
            "spawn",
        );
        keys.push(key);
        common::await_number(&mark);
        beats.push(common::beat_path(&mark));
    }
    let grandchild_mark = dir.path().join("grandchild.pid");
    common::await_number(&grandchild_mark);
    beats.push(common::beat_path(&grandchild_mark));

    assert_eq!(host.owned(), 3);
    let expected_pids: BTreeSet<u32> = keys.iter().map(|key| key.pid).collect();
    assert_eq!(
        expected_pids.len(),
        3,
        "the three direct children must have unique process identifiers"
    );
    for beat in &beats {
        assert!(
            common::beating_after_start(beat),
            "anti-vacuity: {} was not running before shutdown",
            beat.display()
        );
    }
    assert!(
        matches!(reaped.try_recv(), Err(TryRecvError::Empty)),
        "anti-vacuity: no direct child was reaped before shutdown"
    );
    // One grace for all of them, not one each: three children with a 300 ms grace must not take
    // 900 ms. The bound is generous because it is about the shape of the wait, not its speed.
    let started = Instant::now();
    let error = host
        .shutdown()
        .expect_err("a production process group does not prove complete arbitrary-tree coverage");
    let took = started.elapsed();

    assert_containment_unproven(&error);
    assert_eq!(
        host.owned(),
        3,
        "shutdown discarded rows whose boundary coverage stayed incomplete"
    );
    // The discriminator. Each of these watches for roughly 300 ms starting now, so a deferral
    // longer than one heartbeat leaves a counter moving inside the window and fails here.
    for beat in &beats {
        assert!(
            !common::beating(beat),
            "{} was still advancing after shutdown returned: the escalation had not landed",
            beat.display()
        );
    }
    verify_reap_notices(
        &reaped,
        &expected_pids,
        Duration::from_secs(2),
        Duration::from_millis(100),
    )
    .expect("all and only the three direct children were reaped successfully");
    for key in &keys {
        assert_retained_leader_stopped(&host, key);
    }
    // About the shape of the wait and nothing else: it is satisfied by any implementation that
    // returns promptly, including one that returned without doing the work. The counters above are
    // what rule that out.
    assert!(
        took < Duration::from_millis(900),
        "shutdown took {took:?}; the grace is meant to be spent once for all children, not once \
         per child"
    );

    let second = host
        .shutdown()
        .expect_err("a retry without new coverage must retain and report again");
    assert_containment_unproven(&second);
    assert_eq!(host.owned(), 3, "the retry discarded unresolved rows");
}

#[test]
fn real_backend_stops_and_reaps_but_retains_incomplete_boundary() {
    verify_real_backend_shutdown();
}

#[test]
fn retained_real_backend_row_stays_unresolved_on_second_shutdown() {
    verify_real_backend_shutdown();
}

/// Shutting down a host that owns nothing succeeds and does nothing. Trivial, and it is the case a
/// daemon actually hits most often — a clean exit with no agent running — so a `shutdown` that
/// waited a grace for an empty set would cost every quiet exit that grace.
#[test]
fn shutting_down_an_idle_host_is_immediate() {
    let mut host = RealHost::with_grace(Duration::from_secs(30));
    let started = Instant::now();
    host.shutdown().expect("shutdown");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "an idle shutdown waited for a grace it had no reason to wait for"
    );
}
