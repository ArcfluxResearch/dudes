//! [`OneShot`]: the runner for a `Predicate::Command` child, and the three obligations
//! obligations a predicate child carries.
//!
//! A predicate child is a child, so invariant 10 covers it — and the mechanism chosen is a
//! lifetime bound *shorter* than either disjunct of the invariant. The three obligations:
//!
//! 1. **The platform hold is taken at spawn**, not afterwards: [`plat::prepare`] and
//!    [`plat::grip_spawned`], the same pair [`RealHost`](crate::RealHost) applies to an agent
//!    child. There is one hold in this crate and this reuses it.
//! 2. **[`OneShot::run`] kills the group or job and reaps before it returns** — pass, fail or
//!    timeout alike. The wait is bounded by the predicate's own `timeout_s`.
//! 3. **While a predicate is in flight it is reachable from the daemon's exit path**, which is why
//!    [`OneShot::run`] and [`OneShot::sweep`] take `&self` and this type is `Send + Sync`: the
//!    predicate thread and the exit path hold it at once through an `Arc`. Obligation 3 is the one
//!    that holds in the state the daemon is least likely to be in, and it is the reason this type
//!    exists rather than a self-contained call. The sweep also closes the runner, so a predicate
//!    thread that reaches it afterwards starts no child the sweep could not see.
//!
//! # Why this is not `RealHost`, and not `ProcessHost`
//!
//! Obligation 3 rests on a borrow-checker fact rather than a preference: three of `ProcessHost`'s four
//! methods take `&mut self`, so a predicate holding the host for up to `timeout_s` would stall
//! every `Effect::Spawn` and every `Effect::Kill` for that long, and a `Mutex` around the host
//! moves the stall onto the exit path instead of removing it. Two further mismatches are real but
//! secondary: `ProcessHost::spawn` takes a `ManagedInstall` a predicate has none of, and a child in
//! `RealHost`'s table is reaped only by the shutdown sweep, while a predicate's *exit code and
//! output tail* are the evidence and must be read by whoever waits.
//!
//! It is in **this crate** rather than in the daemon so that every child in the process is created
//! by one crate — which is what lets `bins/duded` disallow `std::process::Command` outright, with
//! no allow-listed module. An allow-listed module is a permanent blind spot exactly
//! its own size.
//!
//! # A swept predicate journals nothing
//!
//! `Supervisor::on_predicate_done` stamps `Provenance::Observed` unconditionally —
//! *"`Observed` by construction: the daemon ran it"* — so answering `pass: false` for a run the
//! daemon *interrupted* would journal a fabricated `Observed` verdict, which invariant 3 forbids.
//! That is why [`OneShotOutcome`] is an enum with a [`Swept`](OneShotOutcome::Swept) arm carrying
//! no evidence at all: a caller cannot build a `PredicateResult` out of it, so the goal stays
//! un-judged until the agent's next turn ends and its predicate runs again. Replay does not re-run
//! a swept predicate. Un-judged is the safe direction.
//!
//! # The bounded wait costs a thread, deliberately
//!
//! `Child::wait` has no timeout in `std`, so obligation 2 costs either a helper
//! thread per predicate or a polling loop. This takes the thread. A poll interval is a tunable
//! nobody tunes, and it spends its latency on the exit path — which is exactly the state obligation
//! 3 exists for. One thread per in-flight predicate, and predicates are not concurrent yet.
//!
//! # The sweep goes out before the reap, except where it cannot
//!
//! `src/kill.rs`'s rule — the hard signal goes out *before* [`std::process::Child::wait`], because
//! the group is named by the leader's identifier and the identifier is freed at the reap — holds
//! here on the branch where it matters. On the **timeout** branch the leader is still unreaped when
//! the group is signalled, so the window is closed exactly as it is there.
//!
//! On the branch where the child exited **within** its timeout the waiter thread has already reaped
//! it before the descendant sweep runs. **The pgid is nevertheless still reserved**, and this
//! paragraph used to say otherwise — POSIX.1-2017 XBD 4.14: *"if there exists a process group whose
//! process group ID is equal to that process ID, the process ID shall not be reused by the system
//! until the process group lifetime ends"*, and XBD 3.299 ends that lifetime only when the last
//! process leaves the group. The leader's reap ends the *process* lifetime; the group lives on while
//! any descendant is in it.
//!
//! And the sweep on that branch runs **only under `!quiet(pid)`** — only when the group still has a
//! signallable member — which is exactly the state in which the identifier cannot have been handed
//! to a stranger. So the residual is not a grace-wide window: it is a pid-space wraparound that
//! would have to complete between the guard and the signal.
//!
//! **Removing this sweep is what would be the invariant-10 hole**, on every predicate that leaves a
//! background descendant. The correction matters because a reader who believed the earlier sentence
//! would reach for the wrong fix.

use std::collections::BTreeMap;
use std::io;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use dudes_providers::launch::LaunchSpec;

use crate::kill::Sig;
use crate::{Spawned, plat};
use admission::Admitted;

/// How many bytes of interleaved stdout and stderr are kept for `Evidence::Command.output_tail`.
///
/// A tail rather than the whole stream: the evidence is journaled, and a predicate that prints a
/// megabyte would otherwise put a megabyte in the log for every run of it.
pub const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

/// How often the grace window is re-examined, matching `kill.rs`'s own constant.
const POLL: Duration = Duration::from_millis(5);

/// How often a cancellable run reads its cancel flag while it waits.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// What ends a run besides its child: its timeout, and a flag its caller can set.
struct Bound<'a> {
    timeout: Duration,
    cancel: &'a AtomicBool,
}

impl Bound<'static> {
    /// A run that only its timeout bounds.
    fn timeout(timeout: Duration) -> Self {
        static NEVER: AtomicBool = AtomicBool::new(false);
        Self {
            timeout,
            cancel: &NEVER,
        }
    }
}

/// What one predicate run produced.
///
/// An enum rather than a struct with a flag, and that expresses the obligation in the type: a run
/// the exit path cut short has **no** evidence, so there is nothing for a caller to build a
/// `Record::PredicateResult` out of and the fabricated `Observed` verdict invariant 3 forbids is
/// unrepresentable rather than merely discouraged.
///
/// **Deliberately not `#[non_exhaustive]`**, for the reason `Effect`'s own doc gives: the code that
/// turns an outcome into an `Input::PredicateDone` lives in another crate, so `#[non_exhaustive]`
/// would force it to write `_ => {}` — and a future outcome silently not handled is a goal judged
/// on evidence nobody looked at, or never judged at all. A new variant should fail to compile,
/// loudly, at every site that must handle it.
#[derive(Debug)]
pub enum OneShotOutcome {
    /// The child ran to an end of its own — or to its `timeout_s` — and was reaped.
    Finished {
        /// The exit code; `None` when the child was killed by a signal or by the timeout.
        exit: Option<i32>,
        /// The last [`OUTPUT_TAIL_BYTES`] of stdout and stderr, interleaved as captured, with any
        /// invalid UTF-8 replaced.
        output_tail: String,
        /// Whether the run hit its timeout rather than ending on its own.
        timed_out: bool,
    },
    /// [`OneShot::sweep`] cut this run short, or ran before it started a child. Carries no
    /// evidence, deliberately.
    Swept,
}

/// What [`OneShot::run_captured`] produced.
///
/// A sibling of [`OneShotOutcome`] because its output is a bounded **head** of raw standard output
/// rather than an interleaved tail of text, and a structured read cannot be parsed from a tail.
#[derive(Debug)]
pub enum CapturedOutcome {
    /// The child ended, hit its timeout, or was killed at the output cap, and was reaped.
    Finished {
        /// The exit code; `None` when the child was killed by a signal, by the timeout or at the cap.
        exit: Option<i32>,
        /// The first bytes the child wrote to standard output, at most the cap.
        stdout: Vec<u8>,
        /// Whether the child wrote more than the cap, which killed its group.
        truncated: bool,
        /// Whether the run hit its timeout rather than ending on its own.
        timed_out: bool,
    },
    /// [`OneShot::sweep`] cut this run short, or ran before it started a child.
    Swept,
}

/// The address-space limit [`OneShot::run_captured`] gives its child on Linux. Elsewhere none is
/// applied: macOS does not enforce `RLIMIT_AS`, and Windows has no equivalent here.
pub const CAPTURED_ADDRESS_SPACE_BYTES: u64 = 1 << 30;

/// One predicate child this runner has spawned and not yet finished with.
struct InFlight {
    /// The platform hold taken at spawn: the process group on Unix, the job object on Windows.
    grip: plat::Grip,
    /// Set by [`OneShot::sweep`]. Read by [`OneShot::run`] after its wait, which is how a swept run
    /// is told from one that merely exited quickly.
    swept: bool,
}

impl std::fmt::Debug for InFlight {
    /// Non-exhaustive on purpose, for the reason `Owned`'s impl gives: the platform grip holds
    /// process and job handles whose numeric values say nothing a reader of a trace wants, and
    /// printing them invites treating a handle as an identity.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlight")
            .field("swept", &self.swept)
            .finish_non_exhaustive()
    }
}

/// The children in flight, and whether [`OneShot::sweep`] has closed the runner.
#[derive(Debug, Default)]
struct Registry {
    /// Set by the sweep and never cleared. A run that finds it set starts no child.
    closed: bool,
    runs: BTreeMap<u32, InFlight>,
}

/// In a module of its own so that its field is private to it: [`OneShot::admit`] is the only
/// constructor of an [`Admitted`], even for the rest of this file.
mod admission {
    use std::sync::MutexGuard;

    use super::{InFlight, OneShot, Registry};

    /// The registry lock of a run that found the runner open, held from that check until the run
    /// registers its child.
    ///
    /// Spawning and registering under the lock that checked `closed` is what lets the sweep either
    /// find the child or close the runner before the child existed. `run_spawned` registers
    /// through an `Admitted`, so it cannot be handed a guard that skipped the check. The type does
    /// not stop other code in this file from locking the registry itself, so a release and retake
    /// around the spawn is caught by a test instead:
    /// `a_sweep_during_a_spawn_waits_for_the_registration_and_ends_the_child`.
    pub(super) struct Admitted<'a>(MutexGuard<'a, Registry>);

    impl OneShot {
        /// The registry lock, or `None` once [`OneShot::sweep`] has closed the runner.
        pub(super) fn admit(&self) -> Option<Admitted<'_>> {
            let registry = self.registry();
            if registry.closed {
                None
            } else {
                Some(Admitted(registry))
            }
        }
    }

    impl Admitted<'_> {
        /// Record a spawned child where the sweep finds it, and release the lock.
        pub(super) fn register(mut self, pid: u32, run: InFlight) {
            self.0.runs.insert(pid, run);
        }
    }
}

/// The one-shot runner: spawns a bounded child, reaps it, and stays reachable while it runs.
///
/// `Send + Sync` and every method takes `&self`, so the daemon holds one behind an `Arc` and both
/// the predicate thread and the exit path reach it. That is obligation 3's mechanism, and a
/// `&mut self` API would reproduce on the exit path exactly the stall that obligation 3 rules out for `RealHost`.
#[derive(Debug)]
pub struct OneShot {
    grace: Duration,
    registry: Mutex<Registry>,
}

impl Default for OneShot {
    fn default() -> Self {
        Self::new()
    }
}

impl OneShot {
    /// A runner with [`DEFAULT_GRACE`](crate::DEFAULT_GRACE) between the polite signal and the hard
    /// one.
    #[must_use]
    pub fn new() -> Self {
        Self::with_grace(crate::DEFAULT_GRACE)
    }

    /// [`OneShot::new`] with a chosen grace. Tests use a short one; the daemon uses the default.
    #[must_use]
    pub fn with_grace(grace: Duration) -> Self {
        Self {
            grace,
            registry: Mutex::new(Registry::default()),
        }
    }

    /// How many predicate children are in flight right now. The count obligation 3 is about.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.registry().runs.len()
    }

    /// Run one predicate child to an end and reap it, bounded by `timeout`.
    ///
    /// The spec is fully resolved by the caller: `argv[0]` is the daemon-resolved, canonicalized
    /// program, `env` is the daemon-owned allowlist, and nothing here reads
    /// an ambient environment or touches a shell (invariant 7).
    ///
    /// Returns only after the child has been reaped and its group or job swept, whether it passed,
    /// failed or timed out — obligation 2. On a runner [`OneShot::sweep`] has closed it starts no
    /// child and answers [`OneShotOutcome::Swept`].
    ///
    /// # Errors
    ///
    /// Whatever building the command, taking the platform hold, or the exec reports; and a
    /// weakened platform hold, which outranks the run's own answer for the reason
    /// `RealHost::spawn` gives — "this runner does not fully own this child" is the decision the
    /// caller has to make first.
    pub fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<OneShotOutcome> {
        // The predicate's exit code and output tail *are* the evidence, so unlike an agent child
        // these are pipes rather than the daemon's own descriptors.
        self.run_cancellable(spec, timeout, &AtomicBool::new(false))
    }

    /// [`OneShot::run`] that also ends early once `cancel` is set: the child's group is killed as
    /// on a timeout, the child is reaped, and the run answers [`OneShotOutcome::Swept`], because
    /// a run its caller abandoned carries no evidence. `cancel` is read every few milliseconds.
    ///
    /// # Errors
    ///
    /// As [`OneShot::run`].
    pub fn run_cancellable(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> io::Result<OneShotOutcome> {
        let capture = Capture::tail();
        let bound = Bound { timeout, cancel };
        let settled = self.run_with(spec, &bound, Stdio::piped, Stdio::piped, &capture)?;
        Ok(match settled {
            None => OneShotOutcome::Swept,
            Some(Settled { exit, timed_out }) => OneShotOutcome::Finished {
                exit,
                output_tail: capture.text(),
                timed_out,
            },
        })
    }

    /// [`OneShot::run`] with standard output and error on the null device, for a diagnostic child
    /// whose exit status is the whole answer. The outcome's `output_tail` is always empty, because
    /// nothing reads the child's output.
    ///
    /// # Errors
    ///
    /// As [`OneShot::run`].
    pub fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> io::Result<OneShotOutcome> {
        let capture = Capture::tail();
        let bound = Bound::timeout(timeout);
        let settled = self.run_with(spec, &bound, Stdio::null, Stdio::null, &capture)?;
        Ok(match settled {
            None => OneShotOutcome::Swept,
            Some(Settled { exit, timed_out }) => OneShotOutcome::Finished {
                exit,
                output_tail: String::new(),
                timed_out,
            },
        })
    }

    /// Run one read-only child and keep the first `stdout_cap` bytes of its standard output.
    ///
    /// Standard error goes to the null device. The child is in its own group, bounded by `timeout`
    /// and swept like any other run, and **its group is killed as soon as it has written more than
    /// the cap**, so a command that prints without end costs the cap and not the timeout. On Linux
    /// the child also gets [`CAPTURED_ADDRESS_SPACE_BYTES`] of address space.
    ///
    /// # Errors
    ///
    /// As [`OneShot::run`].
    pub fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> io::Result<CapturedOutcome> {
        let capture = Capture::Head(Arc::new(Mutex::new(Head::new(stdout_cap))));
        let bound = Bound::timeout(timeout);
        let settled = self.run_with(spec, &bound, Stdio::piped, Stdio::null, &capture)?;
        Ok(match settled {
            None => CapturedOutcome::Swept,
            Some(Settled { exit, timed_out }) => {
                let (stdout, truncated) = capture.head();
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                }
            }
        })
    }

    fn run_with(
        &self,
        spec: &LaunchSpec,
        bound: &Bound<'_>,
        stdout: fn() -> Stdio,
        stderr: fn() -> Stdio,
        capture: &Capture,
    ) -> io::Result<Option<Settled>> {
        if matches!(
            spec.stdin,
            dudes_providers::launch::LaunchInput::Protocol(_)
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "one-shot input cannot use a managed protocol",
            ));
        }
        let mut command = crate::command(spec)?;
        command.stdout(stdout());
        command.stderr(stderr());
        let prep = plat::prepare(&mut command)?;

        // A predicate thread the daemon started just before its exit can get here just after the
        // sweep. The exit sweep waits for a start that is already past this line.
        let Some(admitted) = self.admit() else {
            return Ok(None);
        };
        // The last `?` before a process exists. Past this line every exit goes through the registry
        // and the waiter, for the reason `RealHost::spawn` gives at the same point: `?` drops the
        // `Child`, and dropping a `Child` neither signals it nor reaps it — so an early return
        // would leave a running process this runner holds no record of and `sweep` can never reach.
        let child = crate::spawn_serialized(&mut command)?;
        #[cfg(test)]
        tests::after_spawn(child.id());
        if matches!(capture, Capture::Head(_)) {
            limit_address_space(&child);
        }
        self.run_spawned(admitted, spec, bound, prep, child, capture)
    }

    /// Complete the fresh spawn before another owner can reap its child. Kept separate
    /// so tests can observe exit with NOWAIT at this exact boundary, without a scheduling delay.
    fn run_spawned(
        &self,
        admitted: Admitted<'_>,
        spec: &LaunchSpec,
        bound: &Bound<'_>,
        prep: plat::Prep,
        child: Child,
        capture: &Capture,
    ) -> io::Result<Option<Settled>> {
        let pid = child.id();
        let Spawned {
            grip,
            boundary: _,
            weakened,
        } = plat::grip_spawned(prep, &child);
        admitted.register(pid, InFlight { grip, swept: false });

        let outcome = self.settle(child, pid, spec, bound, capture);
        self.registry().runs.remove(&pid);
        match weakened {
            Some(error) => Err(error),
            None => outcome,
        }
    }

    /// Kill every in-flight predicate's group or job. Idempotent, and safe to call while a
    /// [`OneShot::run`] is in flight.
    ///
    /// This is obligation 3's mechanism: the daemon's exit path calls it in the same sequence as
    /// `ProcessHost::shutdown`, so a predicate that was running when the daemon was asked to stop
    /// does not outlive it. One grace is shared across every in-flight child, as `kill::shutdown`
    /// shares it across every agent child.
    ///
    /// **It signals and does not reap.** Each run's own waiter thread reaps its child; a sweep that
    /// waited for them would make the exit path's cost the slowest predicate's.
    ///
    /// **It closes the runner for good.** A run that has not registered its child by now starts
    /// none and answers [`OneShotOutcome::Swept`], because a child started after this list is taken
    /// would lead its own group with nothing left on the exit path to reach it.
    pub fn sweep(&self) {
        let pids: Vec<u32> = {
            let mut registry = self.registry();
            registry.closed = true;
            for entry in registry.runs.values_mut() {
                entry.swept = true;
            }
            registry.runs.keys().copied().collect()
        };
        if pids.is_empty() {
            return;
        }
        for pid in &pids {
            self.signal(*pid, Sig::Term);
        }
        let deadline = Instant::now() + self.grace;
        while Instant::now() < deadline {
            if pids.iter().all(|pid| self.quiet(*pid)) {
                break;
            }
            thread::sleep(POLL);
        }
        for pid in &pids {
            if !self.quiet(*pid) {
                self.signal(*pid, Sig::Kill);
            }
        }
    }

    /// Wait the child out, sweep its group, reap it, and collect the tail.
    ///
    /// Takes the child by value so that there is exactly one owner of it after the spawn, and so
    /// that [`OneShot::run`] can remove the registry entry on every path out of here — including
    /// the error ones.
    fn settle(
        &self,
        mut child: Child,
        pid: u32,
        spec: &LaunchSpec,
        bound: &Bound<'_>,
        capture: &Capture,
    ) -> io::Result<Option<Settled>> {
        let timeout = bound.timeout;
        // The waiter thread owns the wait, and therefore the reap. Nothing here calls `wait`
        // itself, which is what makes the wait bounded — `Child::wait` has no timeout in `std`.
        // The drains share its channel so that a capture reaching its cap wakes the wait too.
        let (tx, rx) = mpsc::channel();
        let drains = [
            drain(child.stdout.take(), capture, &tx),
            drain(child.stderr.take(), capture, &tx),
        ];

        // Before the wait and after the pipes: a child whose stdin stays open waits on it, and a
        // predicate that waited on stdin would spend its whole timeout doing so.
        crate::feed_stdin(&mut child, spec)?;

        let waiter = thread::Builder::new()
            .name(format!("dudes-predicate-{pid}"))
            .spawn(move || {
                let status = child.wait();
                // The receiver is gone only if this call unwound, in which case nobody is owed it.
                let _ = tx.send(Wake::Exited(status));
            })?;

        let started = Instant::now();
        let mut capped = false;
        let mut cancelled = false;
        let (status, timed_out) = loop {
            let left = timeout.saturating_sub(started.elapsed());
            match rx.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(Wake::Exited(status)) => break (status, false),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if !cancelled && bound.cancel.load(Ordering::Acquire) =>
                {
                    // As on a timeout below: the leader is unreaped, so its group is still ours.
                    self.escalate(pid);
                    cancelled = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {}
                Ok(Wake::Capped) => {
                    // The leader may already be reaped here, so this takes the exit branch's
                    // guard below rather than the timeout branch's unguarded signal.
                    if !capped && !self.quiet(pid) {
                        self.escalate(pid);
                    }
                    capped = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // The leader is still unreaped here — the waiter is blocked inside `wait` — so
                    // the group identifier is still reserved and this signal cannot reach a
                    // stranger. That is `kill.rs`'s rule, on the branch where the window would
                    // otherwise be a whole grace wide.
                    self.escalate(pid);
                    let status = loop {
                        match rx.recv() {
                            Ok(Wake::Exited(status)) => break status,
                            Ok(Wake::Capped) => {}
                            Err(_) => {
                                return Err(io::Error::other(
                                    "the predicate waiter thread went away",
                                ));
                            }
                        }
                    };
                    break (status, true);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("the predicate waiter thread went away"));
                }
            }
        };
        // The waiter has sent, so this costs nothing and leaves no thread behind.
        let _ = waiter.join();

        // Descendants of a child that exited on its own are still in the group. Guarded by `quiet`
        // so that the common case — nothing left — sends no signal at all: see the module docs on
        // the window this narrows and does not close.
        if !timed_out && !self.quiet(pid) {
            self.escalate(pid);
        }

        // The pipes close when the last writer in the group is gone, which the escalation above has
        // just made true; a drain that outlived them would be the one thread this path could leak.
        for handle in drains.into_iter().flatten() {
            let _ = handle.join();
        }

        if cancelled || self.was_swept(pid) {
            return Ok(None);
        }
        let status = status?;
        Ok(Some(Settled {
            exit: status.code(),
            timed_out,
        }))
    }

    /// The polite signal, one grace, then the hard one — for one in-flight child.
    fn escalate(&self, pid: u32) {
        self.signal(pid, Sig::Term);
        let deadline = Instant::now() + self.grace;
        while Instant::now() < deadline {
            if self.quiet(pid) {
                return;
            }
            thread::sleep(POLL);
        }
        self.signal(pid, Sig::Kill);
    }

    /// Signal one in-flight child's group or job. A pid the table no longer holds is a run that has
    /// already finished, and signalling it would be signalling whatever holds that number now.
    fn signal(&self, pid: u32, sig: Sig) {
        let registry = self.registry();
        if let Some(entry) = registry.runs.get(&pid)
            && let Err(error) = plat::signal(&entry.grip, pid, sig)
        {
            tracing::debug!(pid, ?sig, %error, "signalling a predicate child failed");
        }
    }

    /// Whether nothing this child's hold covers could still be signalled. A pid the table no longer
    /// holds is quiet by definition: this runner has nothing left to signal for it.
    fn quiet(&self, pid: u32) -> bool {
        self.registry()
            .runs
            .get(&pid)
            .is_none_or(|entry| plat::quiet(&entry.grip, pid))
    }

    /// Whether [`OneShot::sweep`] reached this run.
    fn was_swept(&self, pid: u32) -> bool {
        self.registry()
            .runs
            .get(&pid)
            .is_some_and(|entry| entry.swept)
    }

    /// The registry, with a poisoned lock read through rather than panicked on: a predicate thread
    /// that panicked must not take the exit path's sweep down with it.
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the waiter and the drains tell [`OneShot::settle`].
enum Wake {
    Exited(io::Result<std::process::ExitStatus>),
    /// A capture holds its cap and the child is still writing.
    Capped,
}

/// How a run ended, before its output is read out of the [`Capture`].
struct Settled {
    exit: Option<i32>,
    timed_out: bool,
}

/// Where a run's output goes.
enum Capture {
    /// The last [`OUTPUT_TAIL_BYTES`] of both streams, for evidence.
    Tail(Arc<Mutex<Tail>>),
    /// The first bytes of standard output, for a structured read.
    Head(Arc<Mutex<Head>>),
}

impl Capture {
    fn tail() -> Self {
        Self::Tail(Arc::new(Mutex::new(Tail::new(OUTPUT_TAIL_BYTES))))
    }

    fn text(&self) -> String {
        match self {
            Self::Tail(tail) => tail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .to_string(),
            Self::Head(_) => String::new(),
        }
    }

    fn head(&self) -> (Vec<u8>, bool) {
        match self {
            Self::Head(head) => {
                let head = head.lock().unwrap_or_else(PoisonError::into_inner);
                (head.bytes.clone(), head.truncated)
            }
            Self::Tail(_) => (Vec::new(), false),
        }
    }

    fn share(&self) -> Self {
        match self {
            Self::Tail(tail) => Self::Tail(Arc::clone(tail)),
            Self::Head(head) => Self::Head(Arc::clone(head)),
        }
    }

    /// Take one chunk. `true` when it is the chunk that overflowed a head.
    fn push(&self, chunk: &[u8]) -> bool {
        match self {
            Self::Tail(tail) => {
                tail.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(chunk);
                false
            }
            Self::Head(head) => head
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(chunk),
        }
    }
}

/// Read one pipe to EOF into the capture. `None` when the pipe was never opened.
///
/// A head that overflows wakes the wait once, and the pipe is then read on, discarding, until the
/// group the wait kills closes it.
fn drain(
    pipe: Option<impl io::Read + Send + 'static>,
    capture: &Capture,
    wake: &mpsc::Sender<Wake>,
) -> Option<thread::JoinHandle<()>> {
    let mut pipe = pipe?;
    let capture = capture.share();
    let wake = wake.clone();
    thread::Builder::new()
        .name("dudes-predicate-output".to_owned())
        .spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if capture.push(&buffer[..read]) {
                            let _ = wake.send(Wake::Capped);
                        }
                    }
                }
            }
        })
        .ok()
}

/// The first `cap` bytes written to it.
#[derive(Debug)]
struct Head {
    bytes: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl Head {
    const fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
            truncated: false,
        }
    }

    /// Keep what fits. `true` the first time something did not.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let room = self.cap - self.bytes.len();
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
        if chunk.len() > room && !self.truncated {
            self.truncated = true;
            return true;
        }
        false
    }
}

/// Give a freshly spawned captured child its address-space limit. Linux only, after the spawn,
/// because setting a limit between `fork` and `exec` needs `unsafe` and this crate's grant is one
/// Windows module. The child is unreaped and the limit survives `exec`, so it binds the real
/// program's image whichever side of the `exec` it lands on. A child that is already gone is not an
/// error.
#[cfg(target_os = "linux")]
fn limit_address_space(child: &Child) {
    use rustix::process::{Resource, Rlimit, prlimit};

    let Some(pid) = crate::unix::as_pid(child.id()) else {
        return;
    };
    let limit = Rlimit {
        current: Some(CAPTURED_ADDRESS_SPACE_BYTES),
        maximum: Some(CAPTURED_ADDRESS_SPACE_BYTES),
    };
    if let Err(error) = prlimit(Some(pid), Resource::As, limit) {
        tracing::warn!(%error, "the address-space limit was not set on a captured child");
    }
}

#[cfg(not(target_os = "linux"))]
const fn limit_address_space(_child: &Child) {}

/// The last `cap` bytes written to it, interleaved in arrival order.
///
/// Bounded on the way in rather than truncated on the way out: a predicate that prints for an hour
/// must not grow the daemon's memory, and the evidence a reader wants from a failing command is the
/// end of the output rather than the start.
#[derive(Debug)]
struct Tail {
    bytes: Vec<u8>,
    cap: usize,
}

impl Tail {
    const fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - self.cap..]);
            return;
        }
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.cap {
            let over = self.bytes.len() - self.cap;
            self.bytes.drain(..over);
        }
    }
}

impl std::fmt::Display for Tail {
    /// Lossy on purpose: a predicate's output is whatever the command printed, and refusing to
    /// record it because one byte was not UTF-8 would lose the evidence rather than protect it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::Tail;

    type AfterSpawn = Box<dyn FnOnce(u32)>;

    thread_local! {
        /// Thread-local so that a hook one test sets runs only in that test's own spawn.
        static AFTER_SPAWN: Cell<Option<AfterSpawn>> = const { Cell::new(None) };
    }

    /// Called by `OneShot::run_with` between its spawn and its registration.
    pub(super) fn after_spawn(pid: u32) {
        if let Some(hook) = AFTER_SPAWN.take() {
            hook(pid);
        }
    }

    /// The tail keeps the *end* of what was written and never grows past its cap — the property
    /// that makes it safe to journal. Both arms: a short write is kept whole (the control, without
    /// which a `push` that dropped everything would pass), a long one is cut from the front.
    #[test]
    fn the_tail_keeps_the_last_bytes_and_no_more() {
        let mut tail = Tail::new(4);
        tail.push(b"ab");
        assert_eq!(
            tail.to_string(),
            "ab",
            "control: a short write is kept whole"
        );
        tail.push(b"cd");
        assert_eq!(tail.to_string(), "abcd", "still under the cap");
        tail.push(b"ef");
        assert_eq!(tail.to_string(), "cdef", "the front is what goes");
        tail.push(b"0123456789");
        assert_eq!(
            tail.to_string(),
            "6789",
            "a single oversized write is cut too"
        );
    }

    #[cfg(unix)]
    mod spawn_guard {
        use std::sync::{Arc, mpsc};
        use std::thread;
        use std::time::Duration;

        use dudes_providers::launch::LaunchSpec;
        use rustix::io::Errno;

        use super::AFTER_SPAWN;
        use crate::{OneShot, OneShotOutcome, unix};

        /// How long the spawning thread waits for a sweep that has started. A sweep that can take
        /// the lock finishes in microseconds, and one that cannot is still waiting after this.
        const HOLD: Duration = Duration::from_secs(1);

        /// Far longer than the sweep needs, so a run that reaches it is a run the sweep missed.
        const TIMEOUT: Duration = Duration::from_secs(10);

        /// A sweep that starts between the spawn and the registration waits for the registration,
        /// then ends the child, and the run answers `Swept`.
        ///
        /// The hook starts the sweep on another thread and holds the spawning thread until that
        /// sweep has finished or has waited [`HOLD`]. With the lock held across the spawn, the sweep
        /// is still waiting when the run registers its child, so it finds the child and kills it.
        /// With the lock released and retaken around the spawn, the sweep finishes first with
        /// nothing to kill, and the child sleeps on until the timeout, which answers `Finished`.
        #[test]
        fn a_sweep_during_a_spawn_waits_for_the_registration_and_ends_the_child() {
            let cwd = tempfile::tempdir().expect("working directory");
            let spec = LaunchSpec {
                argv: vec!["/bin/sleep".into(), "30".into()],
                env: Vec::new(),
                cwd: cwd.path().to_owned(),
                stdin: dudes_providers::launch::LaunchInput::Null,
            };
            let runner = Arc::new(OneShot::with_grace(Duration::from_millis(200)));
            let (spawned, child) = mpsc::channel();
            let sweeper = Arc::clone(&runner);
            AFTER_SPAWN.set(Some(Box::new(move |pid| {
                let (started, sweep_started) = mpsc::channel();
                let (finished, sweep_finished) = mpsc::channel();
                let sweep = thread::spawn(move || {
                    started.send(()).expect("report the start");
                    sweeper.sweep();
                    let _ = finished.send(());
                });
                sweep_started.recv().expect("the sweep thread started");
                let _ = sweep_finished.recv_timeout(HOLD);
                spawned.send((pid, sweep)).expect("hand back the child");
            })));

            let outcome = runner.run(&spec, TIMEOUT).expect("the run answers");
            let (pid, sweep) = child.try_recv().expect("the run spawned a child");
            sweep.join().expect("the sweep thread");

            assert!(
                matches!(outcome, OneShotOutcome::Swept),
                "the sweep reached the child: {outcome:?}"
            );
            assert_eq!(
                rustix::process::test_kill_process(unix::as_pid(pid).expect("child pid")),
                Err(Errno::SRCH),
                "the child at pid {pid} is gone"
            );
            assert_eq!(runner.in_flight(), 0);
        }
    }

    #[cfg(target_os = "macos")]
    mod fresh_group {
        use std::fs;
        use std::io::{Read as _, Write as _};
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command, Stdio};
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use dudes_providers::launch::LaunchSpec;
        use rustix::io::Errno;
        use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

        use crate::oneshot::Capture;
        use crate::{OneShot, plat, unix};

        const BOUND: Duration = Duration::from_secs(30);
        const ROLE: &str = "DUDES_ONESHOT_GROUP_FIXTURE";
        const MARK: &str = "DUDES_ONESHOT_GROUP_MARK";

        fn fixture_command(role: &str, mark: &Path) -> Command {
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command.args([
                "--exact",
                "oneshot::tests::fresh_group::fixture_entry",
                "--nocapture",
            ]);
            command.env_clear().env(ROLE, role).env(MARK, mark);
            command
        }

        #[test]
        fn fixture_entry() {
            let Ok(role) = std::env::var(ROLE) else {
                return;
            };
            let mark = PathBuf::from(std::env::var_os(MARK).expect("fixture mark"));
            match role.as_str() {
                "leader" => {
                    // This fixture deliberately leaves a real descendant when its leader exits.
                    #[allow(clippy::zombie_processes)]
                    let _descendant = fixture_command("descendant", &mark)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .expect("descendant");
                    std::io::stdin()
                        .read_exact(&mut [0])
                        .expect("release leader");
                }
                "descendant" => {
                    fs::write(&mark, std::process::id().to_string()).expect("descendant pid");
                    let beat = mark.with_extension("beat");
                    let staging = mark.with_extension("writing");
                    let mut counter = 0_u64;
                    loop {
                        counter += 1;
                        fs::write(&staging, counter.to_string()).expect("stage heartbeat");
                        fs::rename(&staging, &beat).expect("publish heartbeat");
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                "fast" => {}
                _ => panic!("unknown fixture role"),
            }
            writeln!(std::io::stdout(), "fresh-group-result").expect("result marker");
            std::process::exit(17);
        }

        /// Retains the independently created test group for cleanup even if the grip under test
        /// loses it. A surviving member reserves the group ID; query before signalling, as run does.
        struct Cleanup {
            child: Option<Child>,
            group: Pid,
            descendant: Option<Pid>,
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                if rustix::process::test_kill_process_group(self.group).is_ok() {
                    let _ = rustix::process::kill_process_group(self.group, Signal::KILL);
                }
                if let Some(child) = &mut self.child {
                    if child.try_wait().ok().flatten().is_none() {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                }
                if let Some(pid) = self.descendant {
                    // The orphan is collected by the platform reaper, not Child::wait here.
                    let deadline = Instant::now() + BOUND;
                    while rustix::process::test_kill_process(pid) != Err(Errno::SRCH) {
                        assert!(Instant::now() < deadline, "descendant did not disappear");
                        thread::sleep(Duration::from_millis(5));
                    }
                    let _ = writeln!(std::io::stderr(), "fixture descendant cleanup observed");
                }
            }
        }

        fn prepare_fixture(role: &str, mark: &Path) -> (plat::Prep, Cleanup) {
            let mut command = fixture_command(role, mark);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let prep = plat::prepare(&mut command).expect("prepare own group");
            let child = command.spawn().expect("spawn prepared fixture");
            let group = unix::as_pid(child.id()).expect("nonzero child pid");
            (
                prep,
                Cleanup {
                    child: Some(child),
                    group,
                    descendant: None,
                },
            )
        }

        fn observe_exit_without_reaping(child: &mut Child) {
            let pid = unix::as_pid(child.id()).expect("child pid");
            let (sent, received) = mpsc::channel();
            let exit_thread = thread::spawn(move || {
                let result = rustix::process::waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
                )
                .map(|status| status.map(|status| (status.exited(), status.exit_status())));
                let _ = sent.send(result);
            });
            let observed = received.recv_timeout(BOUND);
            if observed.is_err() {
                child.kill().expect("release blocked exit observer");
            }
            exit_thread.join().expect("exit observer");
            let status = observed
                .expect("fixture exit bound")
                .expect("waitid")
                .expect("exit status");
            assert!(status.0, "fixture exited normally");
            assert_eq!(status.1, Some(17));
            assert_eq!(rustix::process::getpgid(Some(pid)), Err(Errno::SRCH));
        }

        fn number(path: &Path) -> Option<u64> {
            fs::read_to_string(path).ok()?.parse().ok()
        }

        fn await_number(path: &Path, after: u64) -> u64 {
            let deadline = Instant::now() + BOUND;
            loop {
                if let Some(value) = number(path).filter(|value| *value > after) {
                    return value;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture did not report at {}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        /// Wait until the process at `pid` can no longer run — which in practice means `ESRCH`,
        /// because that is what a reaped identifier answers.
        ///
        /// Bounded rather than sampled once, and that is the whole point. [`OneShot`] promises only
        /// that every member of the group has been *signalled* before it returns — `TERM`, then
        /// `KILL` after one grace. `escalate` sends that hard signal and returns without asking
        /// again, `sweep` has the same shape, and `sweep`'s own documentation says the rest: "It
        /// signals and does not reap."
        ///
        /// The leader is gone before the sweep, so this descendant's reaper is the platform's, and
        /// neither the instant it stops executing nor the instant of `ESRCH` is sequenced by
        /// anything in this crate. A single read taken the moment `run_spawned` returns is
        /// therefore a race, and a loaded machine loses it: five of twenty full-crate runs, every
        /// failure `left: Ok(())` against `ESRCH`.
        ///
        /// The two errnos are spelled out here rather than taken from `unix::quiet_checked`, and
        /// the reason is not the one this comment used to give: that function maps `ESRCH` to
        /// absence and `EPERM` to [`Presence::Unsignallable`](crate::unix::Presence), which is
        /// present and not finished, so it never mapped "this pair" to anything. The real reason
        /// stands — `OneShot::escalate` consults that function to decide when to stop waiting, so a
        /// test that asked it the same question would agree with the code under test by
        /// construction and a mutant inside it would silence both at once. What this loop accepts
        /// is `ESRCH`; the `EPERM` arm is for a live process it may not signal, and against this
        /// host's own unreaped zombie the direct form does not produce one — it answers rc=0.
        fn await_finished(pid: Pid) {
            let deadline = Instant::now() + BOUND;
            loop {
                if matches!(
                    rustix::process::test_kill_process(pid),
                    Err(Errno::SRCH | Errno::PERM)
                ) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "live descendant remained after OneShot returned"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn finish(prep: plat::Prep, cleanup: &mut Cleanup, cwd: &Path) {
            let spec = LaunchSpec {
                argv: Vec::new(),
                env: Vec::new(),
                cwd: cwd.to_owned(),
                stdin: dudes_providers::launch::LaunchInput::Null,
            };
            let runner = OneShot::with_grace(Duration::from_millis(200));
            let capture = Capture::tail();
            let settled = runner
                .run_spawned(
                    runner.admit().expect("a fresh runner is open"),
                    &spec,
                    &crate::oneshot::Bound::timeout(BOUND),
                    prep,
                    cleanup.child.take().expect("owned leader"),
                    &capture,
                )
                .expect("fresh exited child keeps its spawned group")
                .expect("nothing swept this run");
            assert!(
                settled.exit == Some(17)
                    && !settled.timed_out
                    && capture.text().contains("fresh-group-result"),
                "original exit and output: {:?} {} {}",
                settled.exit,
                settled.timed_out,
                capture.text()
            );
            assert_eq!(runner.in_flight(), 0, "leader reaped and deregistered");
        }

        #[test]
        fn a_fresh_fast_exit_keeps_its_spawned_group_evidence() {
            let root = tempfile::tempdir().expect("fixture directory");
            let (prep, mut cleanup) = prepare_fixture("fast", &root.path().join("unused.pid"));
            observe_exit_without_reaping(cleanup.child.as_mut().expect("unreaped leader"));
            finish(prep, &mut cleanup, root.path());
        }

        #[test]
        fn a_fast_exit_leader_still_sweeps_its_live_descendant() {
            let root = tempfile::tempdir().expect("fixture directory");
            let mark = root.path().join("descendant.pid");
            let (prep, mut cleanup) = prepare_fixture("leader", &mark);
            assert_eq!(
                rustix::process::getpgid(Some(cleanup.group)),
                Ok(cleanup.group)
            );
            let pid = u32::try_from(await_number(&mark, 0)).expect("descendant pid fits");
            let descendant = unix::as_pid(pid).expect("descendant pid");
            cleanup.descendant = Some(descendant);
            assert_eq!(
                rustix::process::getpgid(Some(descendant)),
                Ok(cleanup.group)
            );
            let beat = mark.with_extension("beat");
            let before = await_number(&beat, 0);
            await_number(&beat, before);
            let child = cleanup.child.as_mut().expect("owned leader");
            child
                .stdin
                .take()
                .expect("release pipe")
                .write_all(&[1])
                .expect("release leader");
            observe_exit_without_reaping(child);
            finish(prep, &mut cleanup, root.path());
            // The heartbeat check this replaces read the counter, slept 200 ms and required it
            // unchanged. It is subsumed: a pid that answers `ESRCH` or `EPERM` executes no user
            // code and publishes nothing. Keeping a timed version of it would have reintroduced,
            // one line below its own fix, the defect the line above removes.
            await_finished(descendant);
        }
    }
}
