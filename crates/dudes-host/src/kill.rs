//! The kill path: select, re-validate, signal, wait out the grace, escalate, sweep, reap —
//! synchronously.
//!
//! # Why the escalation is not a timer
//!
//! The whole escalation happens inside one call. Nothing here arms a callback for later, and that
//! is a requirement rather than a style: a grace timer holds an identifier **by value** across a
//! window it does not own, and the identifier is freed at the reap. Arm a timer, let the child be
//! reaped, and the timer fires at whatever now lives at that number. Invariant 10 wants the same
//! thing from the other direction — every child dies with the daemon — and a process that exits
//! during its own grace window never runs the sweep it scheduled.
//!
//! # The order, and why the sweep comes before the reap
//!
//! On Unix the process group is named by the leader's identifier, so the group is reserved by
//! exactly the same thing that reserves the leader: the leader being unreaped. Reap the leader
//! first and the group identifier is free while descendants are still in it — a group signal after
//! that point can sweep a stranger's group. So the last hard signal always goes out *before*
//! [`std::process::Child::wait`], never after, and the wait during the grace is a query about the
//! group rather than a `try_wait` on the leader, because `try_wait` reaps.
//!
//! # What authorises a signal, and why the two paths do not authorise it the same way
//!
//! Two paths reach [`escalate`], and what holds the identifier still is different on each. The
//! split is [`Guard`]: the same host, the same children and the same invariant get one rule per
//! situation, not one rule per entry point the daemon happens to call.
//!
//! - **A child this host spawned and has not reaped.** The identifier is reserved by that fact
//!   alone: it is freed at the *reap*, so an unreaped child's number cannot have been handed to
//!   anything else and a signal to it cannot reach a stranger. That is a **stronger** guarantee
//!   than the key comparison, not a weaker one, and it is what both [`kill`] and [`shutdown`] use
//!   while the watcher still reports the child unreaped.
//!
//!   What the key still has to answer on this path is not "is this identifier still the child"
//!   but **"is this the child the caller meant"**. A host that reaped child A at pid 100 and then
//!   spawned child B at the same pid must not let a stale kill for A take B down — so the key is
//!   compared once, against the key this host read at A's spawn, to *select*. After that the
//!   reservation authorises every signal and nothing is re-probed.
//!
//!   **The boot identity is part of that selection, and so is every other field of the key.** On
//!   Linux the start value is clock ticks since boot, so it means nothing except beside the boot it
//!   was counted from: a daemon that starts at boot and spawns its first agent at a similar offset
//!   every time is exactly where a pid and a start value collide across two boots. After a reboot
//!   the daemon replays the journal, holds keys from the previous boot and emits a kill for one —
//!   and without the boot half that kill selects a live child of *this* run. That is this crate's
//!   own hazard, reached along the one axis the boot identity exists to close.
//!
//!   A refusal here keeps the child in the host until shutdown. Shutdown then asks the same
//!   authority question from the state that stands at each signal: reservation before reap, key comparison
//!   after a keyed reap, and no signal after a keyless reap.
//!
//! - **A key with no child behind it** — adopted from the journal after a restart, or naming a
//!   child this run has already reaped. Nothing here can lean on a reservation this host never
//!   took, so the key comparison is the entire guarantee, and it runs immediately before **each**
//!   of the two signals rather than once at the top.
//!
//! # The reap is no longer this file's, and the first case is therefore conditional
//!
//! A managed turn's result is parsed out of the child's standard output and its exit status is the
//! other half of that end, so each agent child has a watcher thread that owns the
//! [`std::process::Child`], reads the stream, and reaps — the instant the child exits, rather than
//! at the end of a kill. Two things follow, and both are visible in [`kill_owned`]:
//!
//! - The reservation is a fact with an end now, so it is **asked about** rather than assumed:
//!   `Owned::exit.reaped()` is what says whether the identifier is still this host's, and a child
//!   already reaped is guarded by the key comparison instead. The entry stays in the table either
//!   way because it retains every capability acquired, even when the per-signal authority check later withholds shutdown's
//!   signal from a missing or moved leader.
//! - "The sweep comes before the reap" still holds where it matters, and for the reason above it
//!   is no longer this file that orders it. The **polite** signal goes out with the leader
//!   unreaped — that is what the guard checked, two statements earlier. The **hard** signal goes
//!   out only when `wait_until_quiet` answered false, which for a group target means something
//!   signallable is still inside the group; a non-empty group reserves its identifier, and with it
//!   the leader's number (POSIX.1-2017 XBD 4.14, quoted in full in `src/oneshot.rs`). That is the
//!   same narrowing the predicate runner already rests on, and it — not the reservation — is what
//!   authorises the second signal.
//!
//! The two signals therefore leave windows of **different sizes**, and [`guard_for`] is where that
//! is measured rather than asserted: two statements for the first, the whole grace for the second.
//! An earlier version of this paragraph claimed two statements for both, which was wrong in the
//! direction that matters — it invites widening the grace.

use std::collections::BTreeMap;
use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use dudes_supervisor::host::{
    AdoptionKey, ContainmentUnproven, KillOutcome, Liveness, ProbeAnswer, liveness,
};

use crate::{Owned, plat};

/// The strongest target boundary acquired for one spawn attempt.
///
/// The current real backends report one of the three incomplete boundaries. Tests add `Complete`
/// for the scripted positive control; a future backend that independently proves containment must
/// make that a production variant deliberately. Neither a handle nor an addressable group/job is
/// promoted to it by possession alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Boundary {
    #[allow(
        dead_code,
        reason = "scripted positive control; no real backend proves it yet"
    )]
    Complete,
    #[cfg(unix)]
    ProcessGroup,
    #[cfg(windows)]
    Job,
    LeaderOnly,
}

impl Boundary {
    pub(crate) const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// What single-child cleanup established.
///
/// `Debug` under test only. A test that cannot name which of these it got can report only that
/// something was uncertain, which is how an intermittent failure on this path stayed "cause
/// unknown": the spawn result carries the *setup* reason, and the
/// cleanup's own answer went to a log no test transcript shows.
#[cfg_attr(test, derive(Debug))]
pub(crate) enum Cleanup {
    /// Complete boundary, authorised signals, final whole-target quiet and leader reap all held.
    Confirmed,
    /// At least one required fact was absent. The optional error is cleanup detail; the spawn
    /// result continues to carry the original setup error.
    Unconfirmed(Option<io::Error>),
}

/// One cleanup fact supplied or withheld by the producer matrix.
///
/// Test-only: production derives every fact from its platform grip, process table and watcher.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestCleanupEvidence {
    Complete,
    MissingBoundary,
    SignalWithheld,
    QuietWithheld,
    ReapWithheld,
}

/// How long a child gets between the polite signal and the hard one.
///
/// Five seconds because a coding-agent CLI asked to stop flushes a session file and a transcript
/// first, and a shorter window turns an orderly exit into a corrupt one. Platforms with no polite
/// signal (Windows, where a job object is terminated outright) reach quiescence immediately and do
/// not spend it.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// How often the grace window is re-examined. Short enough that a child that exits promptly does
/// not pay for the whole window, long enough that the poll is not itself the load.
const POLL: Duration = Duration::from_millis(5);

/// Which of the two signals in the escalation this is.
///
/// Two, not a signal number: the host sends exactly these and the platform layer decides what each
/// one is. On Windows both are the same act — a job object has no polite form — and the layer
/// says so rather than pretending to a distinction it does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sig {
    /// Ask. `SIGTERM` on Unix.
    Term,
    /// Take. `SIGKILL` on Unix.
    Kill,
}

/// What one independently addressed signalling operation established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The platform accepted delivery to this target.
    Accepted,
    /// The platform established that this target was already absent.
    #[cfg_attr(
        windows,
        allow(dead_code, reason = "Windows handle operations do not report ESRCH")
    )]
    Absent,
    /// The target exists and this delivery signalled nothing in it.
    ///
    /// A group sweep that walked its group and found no member it would signal. It is
    /// **never** [`Delivery::Absent`] and **never** [`Delivery::Accepted`], and no consumer may
    /// convert it to either, to a [`Cleanup::Confirmed`], or to any Observed absence: the group is
    /// still there, and nothing took the signal.
    #[cfg_attr(
        windows,
        allow(dead_code, reason = "a Windows job terminates or it does not")
    )]
    NothingSignalled,
    /// This grip cannot address this target independently.
    Unavailable,
}

/// What a checked target query established, including the scope of an absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetState {
    /// Something in the addressable target is still present.
    Present,
    /// The whole addressable target is absent.
    WholeAbsent,
    /// Only the direct leader is known absent.
    LeaderAbsent,
}

/// The three questions an escalation asks about one identifier, behind one name.
///
/// **The seam exists so that the escalation's *order* can be driven without a process**, and that
/// is not a convenience. The requirement this crate is named for is a statement about an order —
/// validate, signal, wait, validate, signal — and neither half of its second signal can be reached
/// with a real process here.
///
/// *The hard signal going out* needs a target that outlives the polite one, and no helper role in
/// this crate is one: they all die on `SIGTERM`, so the grace window always ends quiet and the
/// escalation returns early — which is why a `panic!` planted immediately after `wait_until_quiet`
/// returned false would execute in no test at all. Two ways of building such a target were tried
/// from here and both are closed. A role that *handles* `SIGTERM` needs signal installation, which
/// no crate in this workspace's graph offers safely and which is `unsafe` (the host confines
/// that to one module). `SIGSTOP` does not build one either: on macOS a stopped process sent
/// `SIGTERM` is made runnable and terminates — the process group answers `Ok`
/// to a signal-0 probe after the `SIGSTOP` and `EPERM` after the subsequent `SIGTERM`. That `EPERM`
/// was never permission uncertainty: the target had terminated and the group held nothing but an
/// unreaped zombie, which Darwin counts as no signallable member and reports as `EPERM`. The
/// construction still cannot establish quiescence, for the better reason that the target dies
/// instead of outliving the polite signal.
///
/// **A third construction does work but is not built here.** A
/// `/bin/sh -c 'trap "" TERM; exec "$0" "$@"'` wrapper installs no handler from Rust — `trap` is a
/// shell builtin, and POSIX preserves an ignored disposition across `exec` — and every argv
/// element stays a typed part, so invariant 7 holds. Driven through the adoption path it fails
/// rather than hangs, because [`kill_adopted`] waits on nothing. This paragraph is here so a reader
/// starts from it rather than from the two closed doors above.
///
/// *The hard signal being withheld* is the separate half, and that one is not reachable with a
/// real process: every construction that makes the second comparison fail makes the first one fail
/// too, and the escalation then stops before it signals anything.
///
/// So the scripted target below is what pins the second signal today.
pub(crate) trait Target {
    /// What the process table says about this identifier right now.
    fn probe(&self) -> ProbeAnswer;
    /// Send one signal to this identifier and everything grouped with it.
    fn signal(&self, sig: Sig) -> io::Result<()>;
    /// Whether anything this target covers could still be signalled.
    fn quiet(&self) -> bool;
    /// [`Self::quiet`] with platform query errors preserved.
    fn checked_quiet(&self) -> io::Result<bool> {
        Ok(self.quiet())
    }
}

/// The additional observation failed-spawn cleanup needs from a target.
trait CleanupTarget: Target {
    /// Query the whole boundary this target claims to cover, preserving query failure as data.
    fn quiet_checked(&self) -> io::Result<bool>;
}

/// The real target: a platform grip and the identifier it covers.
pub(crate) struct PlatTarget<'a> {
    grip: &'a plat::Grip,
    pid: u32,
}

impl<'a> PlatTarget<'a> {
    /// Borrow a grip and the identifier it was taken for.
    pub(crate) const fn new(grip: &'a plat::Grip, pid: u32) -> Self {
        Self { grip, pid }
    }
}

impl Target for PlatTarget<'_> {
    fn probe(&self) -> ProbeAnswer {
        plat::probe(self.grip, self.pid)
    }

    fn signal(&self, sig: Sig) -> io::Result<()> {
        plat::signal(self.grip, self.pid, sig)
    }

    fn quiet(&self) -> bool {
        plat::quiet(self.grip, self.pid)
    }

    fn checked_quiet(&self) -> io::Result<bool> {
        plat::quiet_checked(self.grip, self.pid)
    }
}

impl CleanupTarget for PlatTarget<'_> {
    fn quiet_checked(&self) -> io::Result<bool> {
        plat::quiet_checked(self.grip, self.pid)
    }
}

/// The leader-reap half of cleanup, behind the same internal seam as [`Target`].
trait LeaderExit {
    fn reaped(&self) -> bool;
    fn wait(&mut self) -> io::Result<()>;
}

impl LeaderExit for crate::stream::ExitSlot {
    fn reaped(&self) -> bool {
        crate::stream::ExitSlot::reaped(self)
    }

    fn wait(&mut self) -> io::Result<()> {
        crate::stream::ExitSlot::wait(self)
    }
}

/// What authorises each signal an escalation sends. See the module docs.
#[derive(Clone, Copy)]
pub(crate) enum Guard<'a> {
    /// This host spawned the target and has not reaped it, so the identifier cannot name a
    /// stranger. Admits unconditionally and probes nothing.
    Reserved,
    /// This host holds only a key. The comparison is the whole guarantee and runs before each
    /// signal.
    Key {
        /// What the daemon recorded about the child.
        key: &'a AdoptionKey,
        /// This host's boot identity, or `None` — see [`crate::BootIdentity`].
        host_boot: Option<&'a str>,
    },
}

impl Guard<'_> {
    /// Whether this guard admits a signal to `target` *now*. The error is what [`liveness`]
    /// answered instead, and it is never [`Liveness::Alive`].
    fn admits<T: Target>(self, target: &T) -> Result<(), Liveness> {
        match self {
            Self::Reserved => Ok(()),
            Self::Key { key, host_boot } => match liveness(key, host_boot, target.probe()) {
                Liveness::Alive => Ok(()),
                refused => Err(refused),
            },
        }
    }
}

/// Clean one failed spawn, using only that spawn's `Owned` entry.
///
/// The guard is selected again before each signal because the watcher may reap between them. A
/// reaped keyless child authorises no further signal. Even after a hard signal, quiet is queried
/// again; signal success and leader reap are never promoted into whole-target absence.
pub(crate) fn cleanup_failed_spawn(
    owned: &mut Owned,
    host_boot: Option<&str>,
    grace: Duration,
) -> Cleanup {
    cleanup_failed_spawn_with_boundary(owned, owned.boundary, host_boot, grace)
}

#[cfg(test)]
pub(crate) fn cleanup_failed_spawn_with_evidence_for_test(
    owned: &mut Owned,
    host_boot: Option<&str>,
    grace: Duration,
    evidence: TestCleanupEvidence,
    hold: Option<crate::reap_hold::Hold>,
) -> Cleanup {
    let Owned {
        pid,
        exit,
        grip,
        boundary,
        key,
        ..
    } = owned;
    let target = TestCleanupTarget {
        inner: PlatTarget::new(grip, *pid),
        signal_withheld: evidence == TestCleanupEvidence::SignalWithheld,
        quiet_withheld: evidence == TestCleanupEvidence::QuietWithheld,
        force_quiet: evidence == TestCleanupEvidence::Complete,
        hold,
    };
    let mut exit = TestLeaderExit {
        inner: exit,
        reap_withheld: evidence == TestCleanupEvidence::ReapWithheld,
    };
    let boundary = if evidence == TestCleanupEvidence::MissingBoundary {
        *boundary
    } else {
        Boundary::Complete
    };
    cleanup_target(&target, &mut exit, key.as_ref(), boundary, host_boot, grace)
}

#[cfg(test)]
struct TestCleanupTarget<'a> {
    inner: PlatTarget<'a>,
    signal_withheld: bool,
    quiet_withheld: bool,
    force_quiet: bool,
    /// A reap this cleanup's caller is holding back, released by [`Target::signal`].
    ///
    /// **The release point is the whole of it.** `cleanup_target` reads `exit.reaped()` for its
    /// guard *before* it signals, and the confirmed path ends in `exit.wait()`, which cannot return
    /// until the watcher has reaped. The signal sits between the two: the guard read is already
    /// done, so the ordering it observed is the one the caller forced, and the wait is still ahead,
    /// so it is not left waiting on a watcher this latch is still holding.
    hold: Option<crate::reap_hold::Hold>,
}

#[cfg(test)]
impl Target for TestCleanupTarget<'_> {
    fn probe(&self) -> ProbeAnswer {
        self.inner.probe()
    }

    fn signal(&self, sig: Sig) -> io::Result<()> {
        // Before the withheld branch, not after. A withheld signal still means the guard read has
        // happened, and a latch left holding there would strand the watcher for the rest of the
        // test rather than fail it.
        if let Some(hold) = &self.hold {
            hold.release();
        }
        if self.signal_withheld {
            Err(io::Error::other("test withheld the cleanup signal"))
        } else {
            self.inner.signal(sig)
        }
    }

    fn quiet(&self) -> bool {
        self.inner.quiet()
    }
}

#[cfg(test)]
impl CleanupTarget for TestCleanupTarget<'_> {
    fn quiet_checked(&self) -> io::Result<bool> {
        if self.quiet_withheld {
            Err(io::Error::other(
                "test withheld the whole-target quiet fact",
            ))
        } else if self.force_quiet {
            Ok(true)
        } else {
            self.inner.quiet_checked()
        }
    }
}

#[cfg(test)]
struct TestLeaderExit<'a> {
    inner: &'a mut crate::stream::ExitSlot,
    reap_withheld: bool,
}

#[cfg(test)]
impl LeaderExit for TestLeaderExit<'_> {
    fn reaped(&self) -> bool {
        self.inner.reaped()
    }

    fn wait(&mut self) -> io::Result<()> {
        if self.reap_withheld {
            Err(io::Error::other("test withheld the leader reap fact"))
        } else {
            self.inner.wait()
        }
    }
}

fn cleanup_failed_spawn_with_boundary(
    owned: &mut Owned,
    boundary: Boundary,
    host_boot: Option<&str>,
    grace: Duration,
) -> Cleanup {
    let target = PlatTarget::new(&owned.grip, owned.pid);
    cleanup_target(
        &target,
        &mut owned.exit,
        owned.key.as_ref(),
        boundary,
        host_boot,
        grace,
    )
}

fn cleanup_target<T: CleanupTarget, E: LeaderExit>(
    target: &T,
    exit: &mut E,
    key: Option<&AdoptionKey>,
    boundary: Boundary,
    host_boot: Option<&str>,
    grace: Duration,
) -> Cleanup {
    let Some(guard) = failed_spawn_guard(exit.reaped(), key, host_boot) else {
        return Cleanup::Unconfirmed(None);
    };
    if guard.admits(target).is_err() {
        return Cleanup::Unconfirmed(None);
    }
    if let Err(error) = target.signal(Sig::Term) {
        return Cleanup::Unconfirmed(Some(error));
    }

    let quiet = match wait_until_quiet_checked(target, grace) {
        Ok(quiet) => quiet,
        Err(error) => return Cleanup::Unconfirmed(Some(error)),
    };
    if !quiet {
        let Some(guard) = failed_spawn_guard(exit.reaped(), key, host_boot) else {
            return Cleanup::Unconfirmed(None);
        };
        if guard.admits(target).is_err() {
            return Cleanup::Unconfirmed(None);
        }
        if let Err(error) = target.signal(Sig::Kill) {
            return Cleanup::Unconfirmed(Some(error));
        }
        match target.quiet_checked() {
            Ok(true) => {}
            Ok(false) => return Cleanup::Unconfirmed(None),
            Err(error) => return Cleanup::Unconfirmed(Some(error)),
        }
    }

    if let Err(error) = exit.wait() {
        return Cleanup::Unconfirmed(Some(error));
    }
    if boundary.is_complete() {
        Cleanup::Confirmed
    } else {
        Cleanup::Unconfirmed(None)
    }
}

const fn failed_spawn_guard<'a>(
    reaped: bool,
    key: Option<&'a AdoptionKey>,
    host_boot: Option<&'a str>,
) -> Option<Guard<'a>> {
    if reaped {
        match key {
            Some(key) => Some(Guard::Key { key, host_boot }),
            None => None,
        }
    } else {
        Some(Guard::Reserved)
    }
}

fn wait_until_quiet_checked<T: CleanupTarget>(target: &T, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        if target.quiet_checked()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(POLL);
    }
}

/// What an escalation left behind, and the only thing that says whether waiting on the target is
/// bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settled {
    /// The target reached quiescence inside the grace; no hard signal was needed.
    Quiet,
    /// The grace ran out with the target still there, and the hard signal went out. A hard signal
    /// cannot be caught, blocked or ignored, so a wait after this one ends.
    Struck,
    /// The grace ran out with the target still there, and the guard withheld the hard signal.
    /// **Nothing may wait on the target after this:** it is under no obligation to exit.
    Spared(Liveness),
}

/// Re-validate `key` and, only if it is admitted, run the escalation against it.
///
/// Two paths, and the difference between them is what is holding the identifier still; the module
/// docs say which and why. The entry in `children` leaves the map only when the child has actually
/// been reaped and no native boundary check still needs its row. A refused or failed kill leaves it
/// owned, so [`crate::RealHost::shutdown`] still reaches it.
pub(crate) fn kill(
    children: &mut BTreeMap<u32, Owned>,
    key: &AdoptionKey,
    host_boot: Option<&str>,
    grace: Duration,
    retain_reaped: bool,
) -> KillOutcome {
    match children.remove(&key.pid) {
        Some(owned) => {
            let (outcome, keep) = kill_owned(owned, key, host_boot, grace, retain_reaped);
            if let Some(owned) = keep {
                children.insert(key.pid, owned);
            }
            outcome
        }
        None => kill_adopted(key, host_boot, grace),
    }
}

/// The escalation against a child this host owns. Returns the child to the caller unless it was
/// reaped and the caller has no use for the row. `retain_reaped` is that use: a reaped row keeps
/// the group capability, boundary and reap confirmation that a later whole-group absence check
/// needs, exactly as a child that exited on its own keeps them until shutdown.
///
/// **Which guard applies is decided here rather than assumed**, and that is what the watcher costs.
/// The reservation is the fact that *this host has not reaped the child*, and the
/// reap happens on the child's own watcher thread the instant the child exits — not at the end of
/// this function. So the reservation is asserted only while [`ExitSlot::reaped`] still says the
/// identifier is held; past that the entry is still in the table (its group may still hold
/// descendants, and `shutdown` must reach them) but the identifier is not this host's any more,
/// and the key comparison — probe, compare the whole key, refuse anything that is not `Alive` — is
/// the whole guarantee, exactly as it is for a key adopted from the journal.
///
/// The window this leaves is **not** the same size for the two signals the escalation sends, and
/// [`guard_for`] is where the difference is written down.
fn kill_owned(
    mut owned: Owned,
    key: &AdoptionKey,
    host_boot: Option<&str>,
    grace: Duration,
    retain_reaped: bool,
) -> (KillOutcome, Option<Owned>) {
    if let Err(liveness) = names_the_owned_child(&owned, key) {
        return (KillOutcome::Refused { liveness }, Some(owned));
    }
    let pid = owned.pid;
    let guard = guard_for(owned.exit.reaped(), key, host_boot);
    let target = PlatTarget::new(&owned.grip, pid);
    let (outcome, keep) = kill_selected(&target, &mut owned.exit, guard, grace);
    let reaped = matches!(outcome, KillOutcome::Signalled);
    (
        outcome,
        (keep || (retain_reaped && reaped)).then_some(owned),
    )
}

/// Run an already-selected owned child through escalation and say whether its row remains owned.
/// The caller alone moves the row: this function separates the platform result from map mutation,
/// which keeps the failure path testable without exposing a production fault hook.
fn kill_selected<T: Target, E: LeaderExit>(
    target: &T,
    exit: &mut E,
    guard: Guard<'_>,
    grace: Duration,
) -> (KillOutcome, bool) {
    let settled = match escalate(target, guard, grace) {
        Ok(settled) => settled,
        Err(halt) => return (kill_outcome(halt), true),
    };
    match settled {
        // Only now, with the group swept, is it safe to release the identifier — and the wait is
        // bounded only because one of these two holds: the target is already quiet, or a hard
        // signal has gone out and a hard signal cannot be caught, blocked or ignored. A child the
        // watcher has already reaped answers immediately, which is why "wait" here is never a
        // second wait on a process that has gone.
        Settled::Quiet | Settled::Struck => match exit.wait() {
            Ok(()) => (KillOutcome::Signalled, false),
            Err(error) => (KillOutcome::Failed(error), true),
        },
        // Reachable only on the key-guarded arm above: the reservation admits every signal. That
        // arm is why nothing waits here — a target the guard spared is under no obligation to
        // exit, and waiting on it would block a synchronous kill for ever.
        Settled::Spared(liveness) => (KillOutcome::Refused { liveness }, true),
    }
}

/// Which guard authorises the signals for a child this host holds, given whether its watcher has
/// already reaped it.
///
/// A function rather than an `if` at the call site because it is the one decision the watcher thread added to
/// this path and it is worth being able to drive on its own: `the_guard_is_the_reservation_until_
/// the_watcher_has_reaped` runs [`escalate`] under both answers against a scripted target and
/// separates them by what each *does* — the reservation signals without asking, the key comparison
/// probes first and refuses a target that is not `Alive`. Written as an inline `if`, both branches
/// could be forced and the whole suite stayed green.
///
/// # The exposure this leaves, measured rather than asserted
///
/// **For the first signal it is two statements.** The reservation is read once, here, and the
/// watcher stores its flag as the very next thing after `wait` returns (`src/stream.rs`), so a
/// `Reserved` verdict can be at most that stale when the polite signal goes out.
///
/// **For the second signal it is the whole grace, and saying otherwise was wrong.** The guard is
/// chosen once, before [`escalate`]; [`Guard::Reserved`]'s `admits` is unconditional, so the
/// re-evaluation before the hard signal is a no-op; and `DEFAULT_GRACE` is five seconds. A
/// `SIGKILL` can therefore go out five seconds after the `reaped()` read, on a reservation that
/// ended during the wait. Before the watcher thread existed this could not happen — the only thing that reaped an
/// owned child was this function, at the end — which is exactly why `Reserved` was allowed to skip
/// the per-signal re-probe that [`Guard::Key`] requires.
///
/// **What carries the second signal is not the reservation.** It is the line above it: the hard
/// signal fires *only* when `wait_until_quiet` returned false, and for a group target that means
/// something signallable is still inside the group — which reserves the group identifier, and with
/// it the leader's number (POSIX.1-2017 XBD 4.14, quoted in full in `src/oneshot.rs`). That is the
/// same narrowing the predicate runner already rests on, and it is stated here rather than beside
/// the self-exit branch.
///
/// So the comparison against the macOS probe-to-signal window this crate already accepts is not
/// one-sided, and both sides are worth writing down. This window is **wider** in scope — every
/// platform, not macOS alone — and **weaker** at the moment of the signal, because no comparison
/// runs at all. It is **narrower** in what harm requires: a recycled identifier that is *also* a
/// group leader, while that group is non-quiet. The last dominates, so invariant 10 is not
/// weakened — but a reader who believes the window is two statements wide will change
/// `wait_until_quiet` or the grace default without thinking, and both silently widen it.
const fn guard_for<'a>(
    reaped: bool,
    key: &'a AdoptionKey,
    host_boot: Option<&'a str>,
) -> Guard<'a> {
    if reaped {
        Guard::Key { key, host_boot }
    } else {
        Guard::Reserved
    }
}

/// Whether `key` names the child this host is holding at that identifier.
///
/// **Selection, not liveness** — and still a three-valued answer for it, a [`Liveness`] and never
/// a `bool`.
/// The identifier is reserved by the child being unreaped, so nothing below asks whether a process
/// is still there; it asks which process the caller meant. That makes this a comparison between
/// **two recorded facts** — the key read at spawn and the key the caller passed — rather than
/// between a key and a probe. The no-derive rule on keys is about the key-to-probe comparison,
/// whose answer depends on host state that is not in either key; it is untouched, and
/// [`liveness`] remains the only place that comparison happens.
///
/// It can never wrongly refuse a legitimate caller: a key for a child this host owns came from
/// that child's own `spawn`, so it carries these very values.
///
/// The order is **presence before value**, because the diagonal is what keeps a
/// key this platform could not have emitted from being read as evidence of absence.
fn names_the_owned_child(owned: &Owned, key: &AdoptionKey) -> Result<(), Liveness> {
    // No key was read at spawn, so this host cannot say which child it is holding and will not
    // guess. Shutdown may still signal it on the unreaped reservation; after reap it also refuses.
    let Some(spawned) = &owned.key else {
        return Err(Liveness::Unknown);
    };
    match (spawned.boot.as_deref(), key.boot.as_deref()) {
        // Two *present* boot identities that differ: the key was recorded before a reboot, so the
        // process it names went with that boot. Positive evidence of absence — and the reason this
        // comparison exists, because a boot-relative start value can otherwise collide across two
        // boots and select a live child of this run.
        (Some(recorded), Some(passed)) if recorded != passed => return Err(Liveness::Gone),
        (Some(_), Some(_)) | (None, None) => {}
        // Presence disagreement in either direction. This host's own keys carry a boot identity or
        // do not, uniformly, so such a key is one it could not have emitted: evidence about
        // nothing, and never `Gone` (invariant 3).
        (Some(_), None) | (None, Some(_)) => return Err(Liveness::Unknown),
    }
    if spawned.start_time == key.start_time {
        Ok(())
    } else {
        // A different start value at a reserved identifier means the key is about an *earlier*
        // process at this number — one this host has already reaped, so it is positively gone,
        // and the child standing here now is a different agent's.
        Err(Liveness::Gone)
    }
}

/// The escalation against a key with no child behind it — after a restart, or after this run
/// already reaped the child the key names.
///
/// This is the case a deferred kill after a restart hits. Nothing here can lean on a reservation this host
/// never took, so the key comparison is the entire guarantee, and it is applied before each of the
/// two signals rather than once at the top.
fn kill_adopted(key: &AdoptionKey, host_boot: Option<&str>, grace: Duration) -> KillOutcome {
    // Taken *first*, before the comparison: on the platforms that have one, this is what stops the
    // identifier moving between the answer and the signal.
    let grip = plat::grip_adopted(key.pid);
    let guard = Guard::Key { key, host_boot };
    match escalate(&PlatTarget::new(&grip, key.pid), guard, grace) {
        // A signal went out in all three cases and the polite one cannot be un-sent; `Spared`
        // means only that the hard one was withheld. Nothing waits here — this host is not the
        // target's parent and has nothing to reap — so `Spared` cannot block anything.
        Ok(_) => KillOutcome::Signalled,
        Err(halt) => kill_outcome(halt),
    }
}

/// Why an escalation stopped before it sent anything.
#[derive(Debug)]
enum Halt {
    /// The guard did not admit the first signal, so nothing was signalled.
    Refused(Liveness),
    /// A signal was admitted and a platform signal or checked quiet query failed.
    Failed(io::Error),
}

/// Render the platform escalation's two stop reasons on the host boundary.
///
/// Keeping this conversion beside [`Halt`] makes both owned and adopted callers return the same
/// public result for a refused comparison or a platform operation failure.
fn kill_outcome(halt: Halt) -> KillOutcome {
    match halt {
        Halt::Refused(liveness) => KillOutcome::Refused { liveness },
        Halt::Failed(error) => KillOutcome::Failed(error),
    }
}

/// Polite signal, one grace, hard signal — with the guard consulted immediately before **each** of
/// them. Reaps nothing: the caller owns that decision, and it must come last.
fn escalate<T: Target>(target: &T, guard: Guard<'_>, grace: Duration) -> Result<Settled, Halt> {
    guard.admits(target).map_err(Halt::Refused)?;
    target.signal(Sig::Term).map_err(Halt::Failed)?;

    if wait_until_quiet(target, grace).map_err(Halt::Failed)? {
        return Ok(Settled::Quiet);
    }

    // The second signal is a second signal, so it gets its own comparison. If the child died
    // during the grace and its identifier was taken by something else in that window, this is
    // what catches it — and the polite signal that already went out cannot be un-sent, which is
    // why the caller is still told `Signalled`.
    match guard.admits(target) {
        Ok(()) => {
            target.signal(Sig::Kill).map_err(Halt::Failed)?;
            Ok(Settled::Struck)
        }
        Err(liveness) => Ok(Settled::Spared(liveness)),
    }
}

/// Poll until nothing this target covers can be signalled any more, or the grace runs out.
///
/// Returns whether it went quiet. A *query*, not a wait: on Unix it asks the process group rather
/// than waiting on the leader, because waiting on the leader reaps it and reaping frees the group
/// identifier while descendants may still be inside it.
fn wait_until_quiet<T: Target>(target: &T, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        if target.checked_quiet()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(POLL);
    }
}

/// Apply the signal authority that stands now to one combined polite shutdown signal.
fn signal_for_shutdown<T: Target>(
    target: &T,
    reaped: bool,
    key: Option<&AdoptionKey>,
    host_boot: Option<&str>,
    sig: Sig,
) -> io::Result<bool> {
    let Some(guard) = failed_spawn_guard(reaped, key, host_boot) else {
        return Ok(false);
    };
    if guard.admits(target).is_err() {
        return Ok(false);
    }
    target.signal(sig)?;
    Ok(true)
}

trait ShutdownChild: Target {
    fn boundary(&self) -> Boundary;
    fn signal_boundary(&self, sig: Sig) -> io::Result<Delivery>;
    fn signal_leader(&self, sig: Sig) -> io::Result<Delivery>;
    fn target_state(&self) -> io::Result<TargetState>;
    fn reaped(&self) -> bool;
    fn key(&self) -> Option<&AdoptionKey>;
    fn wait(&mut self) -> io::Result<()>;
}

impl Target for Owned {
    fn probe(&self) -> ProbeAnswer {
        plat::probe(&self.grip, self.pid)
    }

    fn signal(&self, sig: Sig) -> io::Result<()> {
        plat::signal(&self.grip, self.pid, sig)
    }

    fn quiet(&self) -> bool {
        plat::quiet(&self.grip, self.pid)
    }

    fn checked_quiet(&self) -> io::Result<bool> {
        plat::quiet_checked(&self.grip, self.pid)
    }
}

impl ShutdownChild for Owned {
    fn boundary(&self) -> Boundary {
        self.boundary
    }

    fn signal_boundary(&self, sig: Sig) -> io::Result<Delivery> {
        plat::signal_boundary(&self.grip, sig)
    }

    fn signal_leader(&self, sig: Sig) -> io::Result<Delivery> {
        plat::signal_leader(&self.grip, self.pid, sig)
    }

    fn target_state(&self) -> io::Result<TargetState> {
        plat::target_state(&self.grip, self.pid)
    }

    fn reaped(&self) -> bool {
        self.exit.reaped()
    }

    fn key(&self) -> Option<&AdoptionKey> {
        self.key.as_ref()
    }

    fn wait(&mut self) -> io::Result<()> {
        self.exit.wait()
    }
}

fn signal_part_for_shutdown<C, F>(
    child: &C,
    host_boot: Option<&str>,
    operation: F,
) -> io::Result<Delivery>
where
    C: ShutdownChild,
    F: FnOnce(&C) -> io::Result<Delivery>,
{
    let Some(guard) = failed_spawn_guard(child.reaped(), child.key(), host_boot) else {
        return Ok(Delivery::Unavailable);
    };
    if guard.admits(child).is_err() {
        return Ok(Delivery::Unavailable);
    }
    operation(child)
}

#[derive(Default)]
struct ShutdownProgress {
    grace_state: Option<TargetState>,
    final_state: Option<TargetState>,
    grace_query_complete: bool,
    direct_hard_accepted: bool,
    reap_confirmed: bool,
}

pub(crate) const UNRESOLVED_OWNERSHIP: &str = "unresolved ownership remains after shutdown";

fn remember_first(first: &mut Option<io::Error>, error: io::Error) {
    if first.is_none() {
        *first = Some(error);
    }
}

/// Process every retained child under the per-signal authority check, spending **one** grace for all of them, collecting only
/// direct-leader reaps whose waits are independently bounded and retaining every unresolved row.
///
/// The grace is shared rather than paid per child: the polite signals all go out first, the window
/// is waited once, and the hard signals then go to whatever is left. Shutting down twenty agents
/// therefore costs the same wall time as shutting down one.
///
/// Each signal uses the authority that stands at that moment. Ordinary live rows use their
/// reservation. Failed-spawn rows whose watcher already reaped the leader compare an acquired key,
/// or send no signal when no key was acquired. The retained grip is still carried through exit; it
/// is capability, not authority by itself.
pub(crate) fn shutdown(
    children: &mut BTreeMap<u32, Owned>,
    host_boot: Option<&str>,
    grace: Duration,
) -> io::Result<()> {
    shutdown_children(children, host_boot, grace)
}

fn polite_phase<C: ShutdownChild>(
    children: &BTreeMap<u32, C>,
    pids: &[u32],
    host_boot: Option<&str>,
    first_error: &mut Option<io::Error>,
) {
    for pid in pids {
        let child = &children[pid];
        if let Err(error) =
            signal_for_shutdown(child, child.reaped(), child.key(), host_boot, Sig::Term)
        {
            remember_first(first_error, error);
        }
    }
}

fn shared_grace_phase<C: ShutdownChild>(
    children: &BTreeMap<u32, C>,
    pids: &[u32],
    progress: &mut BTreeMap<u32, ShutdownProgress>,
    grace: Duration,
    first_error: &mut Option<io::Error>,
) {
    let deadline = Instant::now() + grace;
    loop {
        let mut any_present = false;
        for pid in pids {
            let state = progress.get_mut(pid).expect("progress for every child");
            if state.grace_query_complete {
                continue;
            }
            match children[pid].target_state() {
                Ok(TargetState::Present) => any_present = true,
                Ok(absent @ (TargetState::WholeAbsent | TargetState::LeaderAbsent)) => {
                    state.grace_state = Some(absent);
                    state.grace_query_complete = true;
                }
                Err(error) => {
                    state.grace_query_complete = true;
                    remember_first(first_error, error);
                }
            }
        }
        if !any_present || Instant::now() >= deadline {
            break;
        }
        sleep(POLL);
    }
}

fn hard_phase<C: ShutdownChild>(
    children: &BTreeMap<u32, C>,
    pids: &[u32],
    progress: &mut BTreeMap<u32, ShutdownProgress>,
    host_boot: Option<&str>,
    first_error: &mut Option<io::Error>,
) {
    for pid in pids {
        let child = &children[pid];
        if progress[pid].grace_state != Some(TargetState::WholeAbsent) {
            if let Err(error) =
                signal_part_for_shutdown(child, host_boot, |child| child.signal_boundary(Sig::Kill))
            {
                remember_first(first_error, error);
            }
            match signal_part_for_shutdown(child, host_boot, |child| child.signal_leader(Sig::Kill))
            {
                Ok(Delivery::Accepted) => {
                    progress
                        .get_mut(pid)
                        .expect("progress for every child")
                        .direct_hard_accepted = true;
                }
                // `NothingSignalled` sits here rather than with `Accepted`: the hard signal reached
                // the group and nothing in it took the signal, so it is not evidence that the
                // direct target accepted anything. It is not an error either, so it records no
                // first error.
                Ok(Delivery::Absent | Delivery::NothingSignalled | Delivery::Unavailable) => {}
                Err(error) => remember_first(first_error, error),
            }
        }
        match child.target_state() {
            Ok(state) => {
                progress
                    .get_mut(pid)
                    .expect("progress for every child")
                    .final_state = Some(state);
            }
            Err(error) => remember_first(first_error, error),
        }
    }
}

fn reap_phase<C: ShutdownChild>(
    children: &mut BTreeMap<u32, C>,
    pids: &[u32],
    progress: &mut BTreeMap<u32, ShutdownProgress>,
    first_error: &mut Option<io::Error>,
) {
    for pid in pids {
        let watcher_finished = children[pid].reaped();
        let state = &progress[pid];
        let wait_eligible = watcher_finished
            || state.final_state == Some(TargetState::WholeAbsent)
            || state.direct_hard_accepted;
        if !wait_eligible {
            continue;
        }
        match children.get_mut(pid).expect("child still retained").wait() {
            Ok(()) => {
                progress
                    .get_mut(pid)
                    .expect("progress for every child")
                    .reap_confirmed = true;
                match children[pid].target_state() {
                    Ok(state) => {
                        progress
                            .get_mut(pid)
                            .expect("progress for every child")
                            .final_state = Some(state);
                    }
                    Err(error) => {
                        progress
                            .get_mut(pid)
                            .expect("progress for every child")
                            .final_state = None;
                        remember_first(first_error, error);
                    }
                }
            }
            Err(error) => remember_first(first_error, error),
        }
    }
}

fn shutdown_children<C: ShutdownChild>(
    children: &mut BTreeMap<u32, C>,
    host_boot: Option<&str>,
    grace: Duration,
) -> io::Result<()> {
    let mut first_error: Option<io::Error> = None;
    let pids: Vec<u32> = children.keys().copied().collect();
    let mut progress: BTreeMap<u32, ShutdownProgress> = pids
        .iter()
        .copied()
        .map(|pid| (pid, ShutdownProgress::default()))
        .collect();

    polite_phase(children, &pids, host_boot, &mut first_error);
    shared_grace_phase(children, &pids, &mut progress, grace, &mut first_error);
    hard_phase(children, &pids, &mut progress, host_boot, &mut first_error);
    reap_phase(children, &pids, &mut progress, &mut first_error);

    let removable: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|pid| {
            let state = &progress[pid];
            children[pid].boundary().is_complete()
                && state.final_state == Some(TargetState::WholeAbsent)
                && state.reap_confirmed
        })
        .collect();
    for pid in removable {
        children.remove(&pid);
    }

    let only_coverage_unproven = children.iter().all(|(pid, child)| {
        let state = &progress[pid];
        child.boundary() != Boundary::LeaderOnly
            && matches!(
                state.final_state,
                Some(TargetState::WholeAbsent | TargetState::LeaderAbsent)
            )
            && state.reap_confirmed
    });
    match first_error {
        Some(error) => Err(error),
        None if children.is_empty() => Ok(()),
        None if only_coverage_unproven => Err(io::Error::other(ContainmentUnproven)),
        None => Err(io::Error::other(UNRESOLVED_OWNERSHIP)),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::kill_selected;
    use super::{
        Boundary, Cleanup, CleanupTarget, Delivery, Guard, Halt, LeaderExit, Settled,
        ShutdownChild, Sig, Target, TargetState, UNRESOLVED_OWNERSHIP, cleanup_target, escalate,
        guard_for, kill_outcome, shutdown_children,
    };
    use dudes_supervisor::host::{AdoptionKey, ContainmentUnproven, Liveness, ProbeAnswer};
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::io;
    use std::time::Duration;

    /// The start value every scripted key below carries.
    const START: u64 = 4_242_424_242;

    /// A grace of zero makes `wait_until_quiet` ask once and give up, so "the target outlived the
    /// grace" costs no wall time and cannot be flaky. A nonzero grace with a target that answers
    /// quiet is the other arm, and also asks once.
    const NO_GRACE: Duration = Duration::ZERO;
    #[cfg(unix)]
    const INCOMPLETE_ADDRESSABLE_BOUNDARY: Boundary = Boundary::ProcessGroup;
    #[cfg(windows)]
    const INCOMPLETE_ADDRESSABLE_BOUNDARY: Boundary = Boundary::Job;

    /// A target whose answers are written down in advance, so the escalation's order is what is
    /// under test rather than a process's timing.
    struct Script {
        /// One answer per [`Target::probe`] call, in order. Running out is a panic: it means the
        /// escalation asked more questions than the case was written for, which is the change the
        /// case exists to notice.
        probes: RefCell<Vec<ProbeAnswer>>,
        /// What [`Target::quiet`] answers every time it is asked.
        quiet: bool,
        /// Every signal that went out, in order. The whole assertion surface.
        sent: RefCell<Vec<Sig>>,
        /// How many times the process table was consulted.
        probed: RefCell<usize>,
        /// One platform signal the target refuses, when this test needs the result boundary.
        failing_signal: Option<Sig>,
        /// One platform signal that fails with a real errno value.
        failing_errno: Option<(Sig, i32)>,
        /// A checked quiet query that fails with a real errno value.
        quiet_errno: Option<i32>,
    }

    impl Script {
        fn new(probes: &[ProbeAnswer], quiet: bool) -> Self {
            Self {
                probes: RefCell::new(probes.iter().copied().rev().collect()),
                quiet,
                sent: RefCell::new(Vec::new()),
                probed: RefCell::new(0),
                failing_signal: None,
                failing_errno: None,
                quiet_errno: None,
            }
        }

        fn failing_signal(mut self, sig: Sig) -> Self {
            self.failing_signal = Some(sig);
            self
        }

        #[cfg(unix)]
        fn failing_errno(mut self, sig: Sig, errno: rustix::io::Errno) -> Self {
            self.failing_errno = Some((sig, errno.raw_os_error()));
            self
        }

        #[cfg(unix)]
        fn failing_quiet_errno(mut self, errno: rustix::io::Errno) -> Self {
            self.quiet_errno = Some(errno.raw_os_error());
            self
        }

        fn sent(&self) -> Vec<Sig> {
            self.sent.borrow().clone()
        }

        fn probed(&self) -> usize {
            *self.probed.borrow()
        }
    }

    impl Target for Script {
        fn probe(&self) -> ProbeAnswer {
            *self.probed.borrow_mut() += 1;
            self.probes
                .borrow_mut()
                .pop()
                .expect("the escalation probed more often than the script allows")
        }

        fn signal(&self, sig: Sig) -> io::Result<()> {
            self.sent.borrow_mut().push(sig);
            if let Some((failing, errno)) = self.failing_errno
                && failing == sig
            {
                return Err(io::Error::from_raw_os_error(errno));
            }
            if self.failing_signal == Some(sig) {
                Err(io::Error::other("scripted platform signal failure"))
            } else {
                Ok(())
            }
        }

        fn quiet(&self) -> bool {
            self.quiet
        }

        fn checked_quiet(&self) -> io::Result<bool> {
            self.quiet_errno.map_or(Ok(self.quiet), |errno| {
                Err(io::Error::from_raw_os_error(errno))
            })
        }
    }

    fn key() -> AdoptionKey {
        AdoptionKey {
            pid: 4242,
            start_time: START,
            boot: None,
        }
    }

    /// A key guard with no boot identity on either side, which is the `(None, None)` cell of
    /// The liveness table: the process table decides.
    fn by_key(key: &AdoptionKey) -> Guard<'_> {
        Guard::Key {
            key,
            host_boot: None,
        }
    }

    /// The watcher thread's one new decision on this path: the guard is the **reservation** until the child's
    /// watcher has reaped it, and the **key comparison** afterwards.
    ///
    /// Separated by what each guard *does* rather than by matching on the variant, so the test is
    /// about behaviour and not about the shape of an `if`. The reservation signals without asking
    /// the process table anything; the key comparison asks first and withholds the signal from a
    /// target the table says is gone. The probe count is what makes the two distinguishable at
    /// all, and it is the assertion the signals cannot replace.
    ///
    /// Both mutations that forced a branch survived the entire suite before this existed —
    /// `guard = Guard::Reserved` always, and `guard = Guard::Key { .. }` always — so each arm
    /// below is aimed at one of them. Forcing `Reserved` makes the second arm signal instead of
    /// refusing; forcing `Key` makes the first arm probe, and `Script` panics on a probe its
    /// script did not allow.
    #[test]
    fn the_guard_is_the_reservation_until_the_watcher_has_reaped() {
        // Unreaped: the identifier is still this host's, so nothing is consulted. The empty probe
        // script is load-bearing — a guard that probed here would panic rather than pass.
        let held = Script::new(&[], true);
        assert_eq!(
            escalate(&held, guard_for(false, &key(), None), NO_GRACE)
                .expect("the reservation admits every signal"),
            Settled::Quiet
        );
        assert_eq!(held.sent(), [Sig::Term]);
        assert_eq!(held.probed(), 0, "the reservation consults nothing");

        // Reaped: the identifier has been released, so the same escalation must compare first and
        // signal nothing at an identifier the table no longer knows.
        let released = Script::new(&[ProbeAnswer::NoSuchPid], true);
        let halt = escalate(&released, guard_for(true, &key(), None), NO_GRACE)
            .expect_err("a released identifier is not signalled");
        assert!(matches!(halt, Halt::Refused(Liveness::Gone)), "{halt:?}");
        assert_eq!(
            released.sent(),
            Vec::<Sig>::new(),
            "nothing was signalled at a released identifier"
        );
        assert_eq!(
            released.probed(),
            1,
            "the key comparison consults the table"
        );
    }

    /// The sentence this crate is named for: the hard signal is sent only after a
    /// **second** comparison of its own.
    ///
    /// Two assertions, and neither replaces the other. The signals pin that the escalation
    /// escalates; the probe count pins that it asked again before doing so. Delete the second
    /// comparison and the signals are unchanged while the count drops to one — which is exactly
    /// the mutation that survived the whole suite before this test existed.
    #[test]
    fn the_hard_signal_follows_a_second_comparison_of_its_own() {
        let key = key();
        let script = Script::new(
            &[
                ProbeAnswer::StartValue(START),
                ProbeAnswer::StartValue(START),
            ],
            false,
        );
        assert_eq!(
            escalate(&script, by_key(&key), NO_GRACE).expect("nothing failed"),
            Settled::Struck
        );
        assert_eq!(script.sent(), vec![Sig::Term, Sig::Kill]);
        assert_eq!(
            script.probed(),
            2,
            "the hard signal went out without a comparison of its own"
        );
    }

    #[test]
    fn a_platform_signal_failure_is_reported_as_kill_outcome_failed() {
        let key = key();
        let script =
            Script::new(&[ProbeAnswer::StartValue(START)], false).failing_signal(Sig::Term);
        let halt = escalate(&script, by_key(&key), NO_GRACE)
            .expect_err("the scripted platform failure reaches the kill result boundary");
        let outcome = kill_outcome(halt);
        match outcome {
            dudes_supervisor::host::KillOutcome::Failed(error) => {
                assert_eq!(error.to_string(), "scripted platform signal failure");
            }
            other => panic!("platform signal failure became {other:?}"),
        }
    }

    /// What the escalation does when it is *handed* a permission denial — which since the
    /// unsignallable classification is no longer the same event as a group answering `EPERM`.
    ///
    /// The two scripted arms below are a refused **delivered** signal and a query that could not be
    /// answered at all. Both are still concrete errors, both still retain the row, and neither is
    /// what a signal-zero query now returns for a group holding an unreaped zombie: that is
    /// `Presence::Unsignallable`, which never reaches this function as an `Err`. The third arm is
    /// what it does reach the cleanup path as — a target that simply never goes quiet.
    #[cfg(unix)]
    #[test]
    fn permission_denied_group_is_retained_or_reported() {
        let script = Script::new(&[], false).failing_errno(Sig::Term, rustix::io::Errno::PERM);
        let mut exit = ScriptedExit::confirmed(false);
        let (outcome, keep) = kill_selected(&script, &mut exit, Guard::Reserved, NO_GRACE);

        match outcome {
            dudes_supervisor::host::KillOutcome::Failed(error) => assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::PERM.raw_os_error())
            ),
            other => panic!("permission denial became {other:?}"),
        }
        assert!(keep, "a failed signal released the owned row");
        assert!(
            exit.wait.is_some(),
            "a failed signal entered the leader wait"
        );

        let query_denied = Script::new(&[], false).failing_quiet_errno(rustix::io::Errno::PERM);
        let mut query_exit = ScriptedExit::confirmed(false);
        let (query_outcome, query_keep) =
            kill_selected(&query_denied, &mut query_exit, Guard::Reserved, NO_GRACE);
        match query_outcome {
            dudes_supervisor::host::KillOutcome::Failed(error) => assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::PERM.raw_os_error())
            ),
            other => panic!("permission-denied signal-zero query became {other:?}"),
        }
        assert!(query_keep, "a failed query released the owned row");
        assert!(
            query_exit.wait.is_some(),
            "a failed query entered the leader wait"
        );

        // The shape an `Unsignallable` query has on the failed-spawn cleanup path: `quiet_checked`
        // answers `Ok(false)` for it, exactly as it does for `Present`, so the target never goes
        // quiet and the cleanup refuses to call it confirmed. Never `Cleanup::Confirmed`, and never
        // an Observed absence.
        let unsignallable = CleanupScript::new(&[], &[QuietAnswer::No, QuietAnswer::No]);
        let mut unsignallable_exit = ScriptedExit::confirmed(false);
        assert!(
            matches!(
                cleanup_target(
                    &unsignallable,
                    &mut unsignallable_exit,
                    None,
                    Boundary::Complete,
                    None,
                    NO_GRACE
                ),
                Cleanup::Unconfirmed(None)
            ),
            "a target that is present but unsignallable was confirmed cleaned up"
        );
    }

    /// The same refusal one level up: a target whose presence query answers `Unsignallable` is
    /// reported and retained, never reported as killed.
    ///
    /// **Why this is not the unit test again.** `unix::tests::only_absence_is_quiet` pins the
    /// mapping; this pins what the mapping *buys*, through `escalate` and `kill_selected`, and the
    /// two fail differently. The target here takes its quiet answer from the production
    /// `unix::quiet_from` rather than from a hard-coded `false`, so moving `Unsignallable` to the
    /// quiet side changes this target's answer and with it the outcome: the escalation would settle
    /// at the grace, report `Signalled` and drop the row, over a group this host has just been told
    /// it cannot signal. The hard signal fails with `EPERM` because this scripted target says so,
    /// and that stands in for the **direct** form: `classify_signal` is unchanged and a direct
    /// delivered denial is still a concrete error. It is no longer what a real *group* sweep of
    /// unsignallable members does — that answers `SignalResult::NothingSignalled`, because `EPERM`
    /// there means the sweep signalled nothing rather than that it was refused. The scripted error is what makes the failure visible instead of silent, and this
    /// test is a firing control precisely because it sits above the platform layer: it reddens if
    /// the new classification is put in `classify_signal` or in `escalate` instead of at the group
    /// form.
    #[cfg(unix)]
    #[test]
    fn an_unsignallable_target_is_reported_and_retained_never_signalled() {
        /// Present, unsignallable, and honest about both.
        struct Unsignallable {
            sent: RefCell<Vec<Sig>>,
        }

        impl Target for Unsignallable {
            fn probe(&self) -> ProbeAnswer {
                panic!("the reservation guard does not probe");
            }

            fn signal(&self, sig: Sig) -> io::Result<()> {
                self.sent.borrow_mut().push(sig);
                match sig {
                    // The polite signal to a group is accepted for whatever it reaches.
                    Sig::Term => Ok(()),
                    // The hard one lands on a group with nothing signallable in it.
                    Sig::Kill => Err(io::Error::from_raw_os_error(
                        rustix::io::Errno::PERM.raw_os_error(),
                    )),
                }
            }

            fn quiet(&self) -> bool {
                crate::unix::quiet_from(crate::unix::Presence::Unsignallable)
            }
        }

        let target = Unsignallable {
            sent: RefCell::new(Vec::new()),
        };
        let mut exit = ScriptedExit::confirmed(false);
        let (outcome, keep) = kill_selected(&target, &mut exit, Guard::Reserved, NO_GRACE);

        match outcome {
            dudes_supervisor::host::KillOutcome::Failed(error) => assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::PERM.raw_os_error()),
                "the unsignallable target failed for some other reason"
            ),
            other => panic!(
                "an unsignallable target was reported as {other:?}; a target this host cannot \
                 signal is never a target it has killed"
            ),
        }
        assert!(
            keep,
            "the ownership row was released over a target that is still present"
        );
        assert_eq!(
            target.sent.borrow().as_slice(),
            &[Sig::Term, Sig::Kill],
            "the escalation did not reach the hard signal, so it never treated the target as alive"
        );
        assert!(
            exit.wait.is_some(),
            "an unsignallable target entered the leader wait"
        );
    }

    /// The other half of the same guarantee, and the half no real process can witness: the target outlived the
    /// grace, so the escalation reached the hard signal — and **withheld** it, because by then the
    /// identifier no longer answered for the recorded child.
    ///
    /// This is the stranger. The second probe finds a process at that identifier with a different
    /// start value, which is precisely a recycle, and an implementation that escalated on
    /// existence rather than on the comparison would kill it.
    #[test]
    fn the_hard_signal_is_withheld_when_the_identifier_has_moved() {
        let key = key();
        let script = Script::new(
            &[
                ProbeAnswer::StartValue(START),
                ProbeAnswer::StartValue(START + 1),
            ],
            false,
        );
        assert_eq!(
            escalate(&script, by_key(&key), NO_GRACE).expect("nothing failed"),
            Settled::Spared(Liveness::Gone)
        );
        assert_eq!(
            script.sent(),
            vec![Sig::Term],
            "the hard signal reached a process that is not the recorded child"
        );
    }

    /// An unreadable second answer is not evidence of anything, so it withholds too: `Unknown` is
    /// not `Alive`, and only `Alive` admits a signal. The `Spared` carries it, so a caller can
    /// tell "the identifier moved" from "I could not tell".
    #[test]
    fn an_unreadable_second_answer_also_withholds_the_hard_signal() {
        let key = key();
        let script = Script::new(
            &[ProbeAnswer::StartValue(START), ProbeAnswer::CouldNotAnswer],
            false,
        );
        assert_eq!(
            escalate(&script, by_key(&key), NO_GRACE).expect("nothing failed"),
            Settled::Spared(Liveness::Unknown)
        );
        assert_eq!(script.sent(), vec![Sig::Term]);
    }

    /// The first comparison gates the *first* signal as well, so a key that has already stopped
    /// matching sends nothing at all — not even the polite signal.
    #[test]
    fn a_key_that_does_not_match_signals_nothing_at_all() {
        let key = key();
        let script = Script::new(&[ProbeAnswer::NoSuchPid], false);
        let halt = escalate(&script, by_key(&key), NO_GRACE).expect_err("must refuse");
        assert!(matches!(halt, Halt::Refused(Liveness::Gone)), "{halt:?}");
        assert_eq!(script.sent(), Vec::new());
        assert_eq!(
            script.probed(),
            1,
            "nothing to re-ask about: no signal went out"
        );
    }

    /// The first comparison's *other* refusal, and the one this crate's Windows kill path now
    /// depends on: an unreadable probe is not evidence, so nothing is signalled and the refusal
    /// says `Unknown` rather than `Gone`.
    ///
    /// Platform-independent by construction, which is the whole reason it is here. `src/windows.rs`
    /// answers [`ProbeAnswer::CouldNotAnswer`] for a process the kernel says has already
    /// terminated, and this pins what the escalation does with that answer — on runners that
    /// cannot execute a line of the Windows layer. The `Unknown` matters beyond the refusal:
    /// `Gone` here would write a `TurnEnded`.
    #[test]
    fn an_unreadable_first_answer_signals_nothing_and_says_unknown() {
        let key = key();
        let script = Script::new(&[ProbeAnswer::CouldNotAnswer], false);
        let halt = escalate(&script, by_key(&key), NO_GRACE).expect_err("must refuse");
        assert!(matches!(halt, Halt::Refused(Liveness::Unknown)), "{halt:?}");
        assert_eq!(script.sent(), Vec::new());
        assert_eq!(script.probed(), 1);
    }

    /// A target that goes quiet inside the grace is never struck, and is never asked about a
    /// second time. This is the ordinary path — a CLI that flushes and exits on the polite signal
    /// — and it is the positive control for the probe counts above: they are not all "2".
    #[test]
    fn a_target_that_goes_quiet_inside_the_grace_is_never_struck() {
        let key = key();
        let script = Script::new(&[ProbeAnswer::StartValue(START)], true);
        assert_eq!(
            escalate(&script, by_key(&key), Duration::from_secs(30)).expect("nothing failed"),
            Settled::Quiet
        );
        assert_eq!(script.sent(), vec![Sig::Term]);
        assert_eq!(script.probed(), 1);
    }

    /// The reservation admits every signal and consults the process table **not at all**.
    ///
    /// That is the whole content of the owned path, and the script is what pins it: `probes` is
    /// empty, so a single probe would panic rather than quietly answer. It is also why
    /// `Settled::Spared` cannot arise there, which is what makes the `wait()` in `kill_owned`
    /// bounded.
    #[test]
    fn the_reservation_admits_every_signal_and_probes_nothing() {
        let script = Script::new(&[], false);
        assert_eq!(
            escalate(&script, Guard::Reserved, NO_GRACE).expect("nothing failed"),
            Settled::Struck
        );
        assert_eq!(script.sent(), vec![Sig::Term, Sig::Kill]);
        assert_eq!(script.probed(), 0);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum QuietAnswer {
        Yes,
        No,
        Error,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CleanupEvent {
        Probe,
        Signal(Sig),
        Quiet,
    }

    struct CleanupScript {
        probes: RefCell<Vec<ProbeAnswer>>,
        quiet: RefCell<Vec<QuietAnswer>>,
        sent: RefCell<Vec<Sig>>,
        events: RefCell<Vec<CleanupEvent>>,
        failing_signal: Option<Sig>,
    }

    impl CleanupScript {
        fn new(probes: &[ProbeAnswer], quiet: &[QuietAnswer]) -> Self {
            Self {
                probes: RefCell::new(probes.iter().copied().rev().collect()),
                quiet: RefCell::new(quiet.iter().copied().rev().collect()),
                sent: RefCell::new(Vec::new()),
                events: RefCell::new(Vec::new()),
                failing_signal: None,
            }
        }

        fn failing_signal(mut self, sig: Sig) -> Self {
            self.failing_signal = Some(sig);
            self
        }

        fn sent(&self) -> Vec<Sig> {
            self.sent.borrow().clone()
        }

        fn events(&self) -> Vec<CleanupEvent> {
            self.events.borrow().clone()
        }
    }

    impl Target for CleanupScript {
        fn probe(&self) -> ProbeAnswer {
            self.events.borrow_mut().push(CleanupEvent::Probe);
            self.probes
                .borrow_mut()
                .pop()
                .expect("cleanup probed more often than scripted")
        }

        fn signal(&self, sig: Sig) -> io::Result<()> {
            self.events.borrow_mut().push(CleanupEvent::Signal(sig));
            self.sent.borrow_mut().push(sig);
            if self.failing_signal == Some(sig) {
                Err(io::Error::other("scripted signal failure"))
            } else {
                Ok(())
            }
        }

        fn quiet(&self) -> bool {
            panic!("failed-spawn cleanup must use the checked quiet query")
        }
    }

    impl CleanupTarget for CleanupScript {
        fn quiet_checked(&self) -> io::Result<bool> {
            self.events.borrow_mut().push(CleanupEvent::Quiet);
            match self
                .quiet
                .borrow_mut()
                .pop()
                .expect("cleanup queried quiet more often than scripted")
            {
                QuietAnswer::Yes => Ok(true),
                QuietAnswer::No => Ok(false),
                QuietAnswer::Error => Err(io::Error::other("quiet query failed")),
            }
        }
    }

    struct ScriptedExit {
        reaped: bool,
        wait: Option<io::Result<()>>,
    }

    impl ScriptedExit {
        fn confirmed(reaped: bool) -> Self {
            Self {
                reaped,
                wait: Some(Ok(())),
            }
        }
    }

    impl LeaderExit for ScriptedExit {
        fn reaped(&self) -> bool {
            self.reaped
        }

        fn wait(&mut self) -> io::Result<()> {
            self.wait
                .take()
                .expect("cleanup waited for the leader more than once")
        }
    }

    /// No intermediate fact is enough. A complete boundary becomes quiet
    /// only after the hard signal and a confirmed reap is `NotLaunched`; withholding either final
    /// quiet or reap remains uncertain.
    #[test]
    fn complete_hold_and_whole_target_quiet_is_not_launched() {
        let script = CleanupScript::new(&[], &[QuietAnswer::No, QuietAnswer::Yes]);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Confirmed
        ));
        assert_eq!(script.sent(), vec![Sig::Term, Sig::Kill]);

        let script = CleanupScript::new(&[], &[QuietAnswer::No, QuietAnswer::No]);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Unconfirmed(None)
        ));

        let script = CleanupScript::new(&[], &[QuietAnswer::Yes]);
        let mut exit = ScriptedExit {
            reaped: false,
            wait: Some(Err(io::Error::other("reap was not confirmed"))),
        };
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Unconfirmed(Some(_))
        ));

        let script = CleanupScript::new(&[], &[]).failing_signal(Sig::Term);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Unconfirmed(Some(_))
        ));
        assert_eq!(script.sent(), vec![Sig::Term]);

        let script = CleanupScript::new(&[], &[QuietAnswer::No]).failing_signal(Sig::Kill);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Unconfirmed(Some(_))
        ));
        assert_eq!(script.sent(), vec![Sig::Term, Sig::Kill]);

        let script = CleanupScript::new(&[], &[QuietAnswer::Error]);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(&script, &mut exit, None, Boundary::Complete, None, NO_GRACE),
            Cleanup::Unconfirmed(Some(_))
        ));
    }

    /// After reap, a missing key authorises no stale signal. Its two controls
    /// demonstrate the reservation before reap and the key comparison after reap.
    #[test]
    fn a_reaped_keyless_failed_spawn_sends_no_stale_signal() {
        let script = CleanupScript::new(&[], &[]);
        let mut exit = ScriptedExit::confirmed(true);
        assert!(matches!(
            cleanup_target(
                &script,
                &mut exit,
                None,
                Boundary::LeaderOnly,
                None,
                NO_GRACE
            ),
            Cleanup::Unconfirmed(None)
        ));
        assert_eq!(script.sent(), Vec::new());

        let script = CleanupScript::new(&[], &[QuietAnswer::Yes]);
        let mut exit = ScriptedExit::confirmed(false);
        assert!(matches!(
            cleanup_target(
                &script,
                &mut exit,
                None,
                Boundary::LeaderOnly,
                None,
                NO_GRACE
            ),
            Cleanup::Unconfirmed(None)
        ));
        assert_eq!(script.sent(), vec![Sig::Term]);

        let key = key();
        let script = CleanupScript::new(&[ProbeAnswer::StartValue(START)], &[QuietAnswer::Yes]);
        let mut exit = ScriptedExit::confirmed(true);
        assert!(matches!(
            cleanup_target(
                &script,
                &mut exit,
                Some(&key),
                Boundary::LeaderOnly,
                None,
                NO_GRACE
            ),
            Cleanup::Unconfirmed(None)
        ));
        assert_eq!(script.sent(), vec![Sig::Term]);
        assert_eq!(
            script.events(),
            vec![
                CleanupEvent::Probe,
                CleanupEvent::Signal(Sig::Term),
                CleanupEvent::Quiet,
            ],
            "the post-reap key comparison must precede the signal"
        );
    }

    struct ShutdownScript {
        reaped: bool,
        key: Option<AdoptionKey>,
        probes: RefCell<Vec<ProbeAnswer>>,
        events: RefCell<Vec<CleanupEvent>>,
        waits: RefCell<usize>,
    }

    impl ShutdownScript {
        fn new(reaped: bool, key: Option<AdoptionKey>, probes: &[ProbeAnswer]) -> Self {
            Self {
                reaped,
                key,
                probes: RefCell::new(probes.iter().copied().rev().collect()),
                events: RefCell::new(Vec::new()),
                waits: RefCell::new(0),
            }
        }

        fn events(&self) -> Vec<CleanupEvent> {
            self.events.borrow().clone()
        }
    }

    impl Target for ShutdownScript {
        fn probe(&self) -> ProbeAnswer {
            self.events.borrow_mut().push(CleanupEvent::Probe);
            self.probes
                .borrow_mut()
                .pop()
                .expect("shutdown probed more often than scripted")
        }

        fn signal(&self, sig: Sig) -> io::Result<()> {
            self.events.borrow_mut().push(CleanupEvent::Signal(sig));
            Ok(())
        }

        fn quiet(&self) -> bool {
            self.events.borrow_mut().push(CleanupEvent::Quiet);
            false
        }
    }

    impl ShutdownChild for ShutdownScript {
        fn boundary(&self) -> Boundary {
            Boundary::Complete
        }

        fn signal_boundary(&self, sig: Sig) -> io::Result<Delivery> {
            self.events.borrow_mut().push(CleanupEvent::Signal(sig));
            Ok(Delivery::Accepted)
        }

        fn signal_leader(&self, sig: Sig) -> io::Result<Delivery> {
            self.events.borrow_mut().push(CleanupEvent::Signal(sig));
            Ok(Delivery::Accepted)
        }

        fn target_state(&self) -> io::Result<TargetState> {
            self.events.borrow_mut().push(CleanupEvent::Quiet);
            Ok(TargetState::Present)
        }

        fn reaped(&self) -> bool {
            self.reaped
        }

        fn key(&self) -> Option<&AdoptionKey> {
            self.key.as_ref()
        }

        fn wait(&mut self) -> io::Result<()> {
            *self.waits.borrow_mut() += 1;
            Ok(())
        }
    }

    /// A retained failed-spawn row reaches the complete ordinary-shutdown loop after its leader may
    /// already have been reaped. The authority check is re-evaluated for both signals: a keyed row probes before
    /// each, a keyless row sends none, every finished watcher result is collected, and the
    /// unreaped reservation is the positive control.
    #[test]
    fn retained_reaped_shutdown_revalidates_or_withholds_each_signal() {
        let key = key();

        let keyless = ShutdownScript::new(true, None, &[]);
        let mut children = BTreeMap::from([(1, keyless)]);
        assert_eq!(
            shutdown_children(&mut children, None, NO_GRACE)
                .unwrap_err()
                .to_string(),
            UNRESOLVED_OWNERSHIP
        );
        assert_eq!(
            children[&1].events(),
            vec![
                CleanupEvent::Quiet,
                CleanupEvent::Quiet,
                CleanupEvent::Quiet
            ]
        );
        assert_eq!(*children[&1].waits.borrow(), 1);

        let moved = ShutdownScript::new(
            true,
            Some(key.clone()),
            &[
                ProbeAnswer::NoSuchPid,
                ProbeAnswer::NoSuchPid,
                ProbeAnswer::NoSuchPid,
            ],
        );
        let mut children = BTreeMap::from([(1, moved)]);
        assert!(shutdown_children(&mut children, None, NO_GRACE).is_err());
        assert_eq!(
            children[&1].events(),
            vec![
                CleanupEvent::Probe,
                CleanupEvent::Quiet,
                CleanupEvent::Probe,
                CleanupEvent::Probe,
                CleanupEvent::Quiet,
                CleanupEvent::Quiet,
            ]
        );

        let matching = ShutdownScript::new(
            true,
            Some(key),
            &[
                ProbeAnswer::StartValue(START),
                ProbeAnswer::StartValue(START),
                ProbeAnswer::StartValue(START),
            ],
        );
        let mut children = BTreeMap::from([(1, matching)]);
        assert!(shutdown_children(&mut children, None, NO_GRACE).is_err());
        assert_eq!(
            children[&1].events(),
            vec![
                CleanupEvent::Probe,
                CleanupEvent::Signal(Sig::Term),
                CleanupEvent::Quiet,
                CleanupEvent::Probe,
                CleanupEvent::Signal(Sig::Kill),
                CleanupEvent::Probe,
                CleanupEvent::Signal(Sig::Kill),
                CleanupEvent::Quiet,
                CleanupEvent::Quiet,
            ]
        );

        let reserved = ShutdownScript::new(false, None, &[]);
        let mut children = BTreeMap::from([(1, reserved)]);
        assert!(shutdown_children(&mut children, None, NO_GRACE).is_err());
        assert_eq!(
            children[&1].events(),
            vec![
                CleanupEvent::Signal(Sig::Term),
                CleanupEvent::Quiet,
                CleanupEvent::Signal(Sig::Kill),
                CleanupEvent::Signal(Sig::Kill),
                CleanupEvent::Quiet,
                CleanupEvent::Quiet,
            ]
        );
        assert_eq!(*children[&1].waits.borrow(), 1);
    }

    struct MixedPermission {
        events: RefCell<Vec<&'static str>>,
        waits: Cell<usize>,
        boundary: Cell<Boundary>,
        state: Cell<TargetState>,
        direct_denied: Cell<bool>,
        leader_gone: Cell<bool>,
        reaped: Cell<bool>,
        polite_error: Cell<Option<&'static str>>,
        query_error: Cell<Option<&'static str>>,
        reap_after_boundary: Cell<bool>,
        final_state: Cell<Option<TargetState>>,
        queries: Cell<usize>,
        wait_error: Cell<Option<&'static str>>,
        after_wait: Cell<Option<Result<TargetState, &'static str>>>,
    }

    impl MixedPermission {
        fn denied() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                waits: Cell::new(0),
                boundary: Cell::new(Boundary::Complete),
                state: Cell::new(TargetState::Present),
                direct_denied: Cell::new(true),
                leader_gone: Cell::new(false),
                reaped: Cell::new(false),
                polite_error: Cell::new(None),
                query_error: Cell::new(None),
                reap_after_boundary: Cell::new(false),
                final_state: Cell::new(None),
                queries: Cell::new(0),
                wait_error: Cell::new(None),
                after_wait: Cell::new(None),
            }
        }

        fn later_row() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                waits: Cell::new(0),
                boundary: Cell::new(INCOMPLETE_ADDRESSABLE_BOUNDARY),
                state: Cell::new(TargetState::WholeAbsent),
                direct_denied: Cell::new(false),
                leader_gone: Cell::new(false),
                reaped: Cell::new(true),
                polite_error: Cell::new(None),
                query_error: Cell::new(None),
                reap_after_boundary: Cell::new(false),
                final_state: Cell::new(None),
                queries: Cell::new(0),
                wait_error: Cell::new(None),
                after_wait: Cell::new(None),
            }
        }

        fn unresolved(boundary: Boundary, state: TargetState, reaped: bool) -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                waits: Cell::new(0),
                boundary: Cell::new(boundary),
                state: Cell::new(state),
                direct_denied: Cell::new(false),
                leader_gone: Cell::new(false),
                reaped: Cell::new(reaped),
                polite_error: Cell::new(None),
                query_error: Cell::new(None),
                reap_after_boundary: Cell::new(false),
                final_state: Cell::new(None),
                queries: Cell::new(0),
                wait_error: Cell::new(None),
                after_wait: Cell::new(None),
            }
        }

        fn with_polite_error(self, error: &'static str) -> Self {
            self.polite_error.set(Some(error));
            self
        }

        fn with_query_error(self, error: &'static str) -> Self {
            self.query_error.set(Some(error));
            self
        }

        fn reaping_after_boundary(self) -> Self {
            self.reap_after_boundary.set(true);
            self
        }

        fn with_final_state(self, state: TargetState) -> Self {
            self.final_state.set(Some(state));
            self
        }

        fn with_wait_error(self, error: &'static str) -> Self {
            self.wait_error.set(Some(error));
            self
        }

        fn with_after_wait(self, answer: Result<TargetState, &'static str>) -> Self {
            self.after_wait.set(Some(answer));
            self
        }

        fn with_leader_gone(self) -> Self {
            self.leader_gone.set(true);
            self
        }
    }

    impl Target for MixedPermission {
        fn probe(&self) -> ProbeAnswer {
            ProbeAnswer::StartValue(START)
        }

        fn signal(&self, _sig: Sig) -> io::Result<()> {
            self.events.borrow_mut().push("polite");
            self.polite_error
                .take()
                .map_or(Ok(()), |error| Err(io::Error::other(error)))
        }

        fn quiet(&self) -> bool {
            false
        }
    }

    impl ShutdownChild for MixedPermission {
        fn boundary(&self) -> Boundary {
            self.boundary.get()
        }

        fn signal_boundary(&self, _sig: Sig) -> io::Result<Delivery> {
            self.events.borrow_mut().push("boundary-hard");
            if self.reap_after_boundary.get() {
                self.reaped.set(true);
            }
            if self.boundary.get() == Boundary::LeaderOnly {
                Ok(Delivery::Unavailable)
            } else {
                Ok(Delivery::Accepted)
            }
        }

        fn signal_leader(&self, _sig: Sig) -> io::Result<Delivery> {
            self.events.borrow_mut().push("leader-hard");
            if self.direct_denied.get() {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "direct leader permission denied",
                ))
            } else if self.leader_gone.get() {
                Ok(Delivery::Absent)
            } else {
                Ok(Delivery::Accepted)
            }
        }

        fn target_state(&self) -> io::Result<TargetState> {
            self.events.borrow_mut().push("query");
            let query = self.queries.replace(self.queries.get() + 1);
            self.query_error.take().map_or_else(
                || {
                    Ok(if query == 0 {
                        self.state.get()
                    } else {
                        self.final_state.get().unwrap_or_else(|| self.state.get())
                    })
                },
                |error| Err(io::Error::other(error)),
            )
        }

        fn reaped(&self) -> bool {
            self.reaped.get()
        }

        fn key(&self) -> Option<&AdoptionKey> {
            None
        }

        fn wait(&mut self) -> io::Result<()> {
            self.waits.set(self.waits.get() + 1);
            if let Some(answer) = self.after_wait.take() {
                match answer {
                    Ok(state) => self.final_state.set(Some(state)),
                    Err(error) => self.query_error.set(Some(error)),
                }
            }
            self.wait_error
                .get()
                .map_or(Ok(()), |error| Err(io::Error::other(error)))
        }
    }

    #[test]
    fn shutdown_refreshes_target_absence_after_an_exit_time_sweep_and_reap() {
        let child = MixedPermission::unresolved(
            INCOMPLETE_ADDRESSABLE_BOUNDARY,
            TargetState::Present,
            false,
        )
        .with_after_wait(Ok(TargetState::WholeAbsent));
        let mut children = BTreeMap::from([(10, child)]);
        let error = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert!(ContainmentUnproven::is(&error), "{error}");
        assert_eq!(children.len(), 1, "incomplete boundary remains owned");
    }

    #[test]
    fn a_failed_post_reap_target_query_remains_a_concrete_cleanup_error() {
        let child = MixedPermission::unresolved(
            INCOMPLETE_ADDRESSABLE_BOUNDARY,
            TargetState::Present,
            false,
        )
        .with_after_wait(Err("post-reap query failed"));
        let mut children = BTreeMap::from([(10, child)]);
        let error = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), "post-reap query failed");
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn aggregate_group_success_does_not_authorize_direct_leader_wait() {
        let mut children = BTreeMap::from([
            (10, MixedPermission::denied()),
            (20, MixedPermission::later_row()),
        ]);

        let error = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(children.keys().copied().collect::<Vec<_>>(), [10, 20]);
        assert_eq!(children[&10].waits.get(), 0);
        assert!(
            children[&20].events.borrow().contains(&"query"),
            "the later row was not attempted after the earlier error"
        );

        let direct = MixedPermission::unresolved(
            INCOMPLETE_ADDRESSABLE_BOUNDARY,
            TargetState::Present,
            false,
        );
        let mut direct_children = BTreeMap::from([(30, direct)]);
        let error = shutdown_children(&mut direct_children, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), UNRESOLVED_OWNERSHIP);
        assert_eq!(
            direct_children[&30].waits.get(),
            1,
            "independent direct hard delivery did not permit exactly one wait"
        );

        let reaped = MixedPermission::unresolved(
            INCOMPLETE_ADDRESSABLE_BOUNDARY,
            TargetState::Present,
            false,
        )
        .reaping_after_boundary();
        let mut reaped_children = BTreeMap::from([(40, reaped)]);
        assert!(shutdown_children(&mut reaped_children, None, NO_GRACE).is_err());
        assert!(
            !reaped_children[&40]
                .events
                .borrow()
                .contains(&"leader-hard"),
            "a reap between group and direct decisions did not re-run the authority check"
        );
    }

    #[test]
    fn retained_unresolved_row_never_returns_success() {
        let mut children = BTreeMap::from([(
            10,
            MixedPermission::unresolved(
                INCOMPLETE_ADDRESSABLE_BOUNDARY,
                TargetState::WholeAbsent,
                true,
            ),
        )]);

        let first = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert!(ContainmentUnproven::is(&first), "{first}");
        assert_eq!(children.keys().copied().collect::<Vec<_>>(), [10]);

        let retained = children.get(&10).expect("the unresolved row remains owned");
        retained.boundary.set(Boundary::Complete);
        retained.state.set(TargetState::WholeAbsent);
        retained.reaped.set(true);

        shutdown_children(&mut children, None, NO_GRACE)
            .expect("complete coverage, fresh absence and reap remove the retained row on retry");
        assert!(children.is_empty());

        let stale_absence =
            MixedPermission::unresolved(Boundary::Complete, TargetState::WholeAbsent, true)
                .with_final_state(TargetState::Present);
        let mut stale = BTreeMap::from([(20, stale_absence)]);
        assert_eq!(
            shutdown_children(&mut stale, None, NO_GRACE)
                .unwrap_err()
                .to_string(),
            UNRESOLVED_OWNERSHIP
        );
        assert!(
            stale.contains_key(&20),
            "a stale grace query removed the row"
        );

        let reap_failed =
            MixedPermission::unresolved(Boundary::Complete, TargetState::WholeAbsent, false)
                .with_wait_error("reap failed");
        let mut unreaped = BTreeMap::from([(30, reap_failed)]);
        assert_eq!(
            shutdown_children(&mut unreaped, None, NO_GRACE)
                .unwrap_err()
                .to_string(),
            "reap failed"
        );
        assert!(unreaped.contains_key(&30), "a failed reap removed the row");
    }

    #[test]
    fn finished_watcher_result_is_collected_before_reap_is_confirmed() {
        let failed =
            MixedPermission::unresolved(Boundary::Complete, TargetState::WholeAbsent, true)
                .with_wait_error("finished watcher reap failed");
        let mut children = BTreeMap::from([(10, failed)]);

        let first = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert_eq!(first.to_string(), "finished watcher reap failed");
        assert_eq!(children[&10].waits.get(), 1);
        let retry = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert_eq!(retry.to_string(), "finished watcher reap failed");
        assert_eq!(children[&10].waits.get(), 2);

        let successful =
            MixedPermission::unresolved(Boundary::Complete, TargetState::WholeAbsent, true);
        let mut success = BTreeMap::from([(20, successful)]);
        shutdown_children(&mut success, None, NO_GRACE)
            .expect("a stored successful reap remains valid completion evidence");
        assert!(success.is_empty());

        let native_incomplete = MixedPermission::unresolved(
            INCOMPLETE_ADDRESSABLE_BOUNDARY,
            TargetState::WholeAbsent,
            true,
        )
        .with_wait_error("native reap failed");
        let mut native = BTreeMap::from([(30, native_incomplete)]);
        let error = shutdown_children(&mut native, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), "native reap failed");
        assert!(native.contains_key(&30));
    }

    #[test]
    fn incomplete_boundary_absence_and_reap_retains_ownership() {
        let mut leader_only = BTreeMap::from([(
            10,
            MixedPermission::unresolved(Boundary::LeaderOnly, TargetState::LeaderAbsent, true),
        )]);
        assert_eq!(
            shutdown_children(&mut leader_only, None, NO_GRACE)
                .unwrap_err()
                .to_string(),
            UNRESOLVED_OWNERSHIP,
            "a leader-only grip has no group or job whose containment is the only thing unproven"
        );
        assert!(leader_only.contains_key(&10));

        #[cfg(unix)]
        {
            let mut group = BTreeMap::from([(
                20,
                MixedPermission::unresolved(
                    INCOMPLETE_ADDRESSABLE_BOUNDARY,
                    TargetState::WholeAbsent,
                    true,
                ),
            )]);
            let error = shutdown_children(&mut group, None, NO_GRACE).unwrap_err();
            assert!(ContainmentUnproven::is(&error), "{error}");
            assert!(group.contains_key(&20));
        }

        #[cfg(windows)]
        {
            let mut job = BTreeMap::from([(
                20,
                MixedPermission::unresolved(Boundary::Job, TargetState::LeaderAbsent, true),
            )]);
            let error = shutdown_children(&mut job, None, NO_GRACE).unwrap_err();
            assert!(ContainmentUnproven::is(&error), "{error}");
            assert!(job.contains_key(&20));
        }
    }

    /// One retained row that still shows something present keeps the ordinary error.
    #[test]
    fn only_rows_missing_just_complete_coverage_report_unproven_containment() {
        let exited = || {
            MixedPermission::unresolved(
                INCOMPLETE_ADDRESSABLE_BOUNDARY,
                TargetState::WholeAbsent,
                true,
            )
        };
        let mut all_exited = BTreeMap::from([(10, exited()), (20, exited())]);
        let error = shutdown_children(&mut all_exited, None, NO_GRACE).unwrap_err();
        assert!(ContainmentUnproven::is(&error), "{error}");
        assert_eq!(
            all_exited.len(),
            2,
            "unproven containment still retains every row"
        );

        let mut one_present = BTreeMap::from([
            (10, exited()),
            (
                20,
                MixedPermission::unresolved(
                    INCOMPLETE_ADDRESSABLE_BOUNDARY,
                    TargetState::Present,
                    false,
                ),
            ),
        ]);
        let error = shutdown_children(&mut one_present, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), UNRESOLVED_OWNERSHIP);
        assert!(!ContainmentUnproven::is(&error));

        let mut reappeared =
            BTreeMap::from([(10, exited().with_final_state(TargetState::Present))]);
        let error = shutdown_children(&mut reappeared, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), UNRESOLVED_OWNERSHIP);

        let mut reap_failed = BTreeMap::from([(10, exited().with_wait_error("reap failed"))]);
        let error = shutdown_children(&mut reap_failed, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), "reap failed");

        // No quiet group, no finished watcher and no accepted direct signal: the wait is never
        // eligible, so the leader's reap stays unconfirmed without any error.
        let mut unreaped = BTreeMap::from([(
            10,
            MixedPermission::unresolved(
                INCOMPLETE_ADDRESSABLE_BOUNDARY,
                TargetState::LeaderAbsent,
                false,
            )
            .with_leader_gone(),
        )]);
        let error = shutdown_children(&mut unreaped, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), UNRESOLVED_OWNERSHIP);
        assert_eq!(unreaped[&10].waits.get(), 0);
    }

    #[test]
    fn shutdown_returns_first_error_in_phase_and_stable_row_order() {
        let mut children = BTreeMap::from([
            (
                10,
                MixedPermission::unresolved(
                    INCOMPLETE_ADDRESSABLE_BOUNDARY,
                    TargetState::Present,
                    false,
                )
                .with_query_error("later query error"),
            ),
            (
                20,
                MixedPermission::unresolved(
                    INCOMPLETE_ADDRESSABLE_BOUNDARY,
                    TargetState::Present,
                    false,
                )
                .with_polite_error("first polite error"),
            ),
            (
                30,
                MixedPermission::unresolved(
                    INCOMPLETE_ADDRESSABLE_BOUNDARY,
                    TargetState::Present,
                    false,
                )
                .with_polite_error("second polite error"),
            ),
        ]);

        let error = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert_eq!(error.to_string(), "first polite error");
        assert!(
            children[&30].events.borrow().contains(&"boundary-hard"),
            "a later row was not attempted after the first error"
        );
        assert_eq!(children.len(), 3, "incomplete coverage was discarded");
    }

    #[cfg(unix)]
    #[test]
    fn held_success_ends_reservation_before_shutdown_retry() {
        struct HeldRetry {
            exit: crate::stream::ExitSlot,
            signals: Cell<usize>,
        }

        impl Target for HeldRetry {
            fn probe(&self) -> ProbeAnswer {
                ProbeAnswer::CouldNotAnswer
            }

            fn signal(&self, _sig: Sig) -> io::Result<()> {
                self.signals.set(self.signals.get() + 1);
                Ok(())
            }

            fn quiet(&self) -> bool {
                false
            }
        }

        impl ShutdownChild for HeldRetry {
            fn boundary(&self) -> Boundary {
                INCOMPLETE_ADDRESSABLE_BOUNDARY
            }

            fn signal_boundary(&self, _sig: Sig) -> io::Result<Delivery> {
                Ok(Delivery::Accepted)
            }

            fn signal_leader(&self, _sig: Sig) -> io::Result<Delivery> {
                Ok(Delivery::Accepted)
            }

            fn target_state(&self) -> io::Result<TargetState> {
                Ok(TargetState::WholeAbsent)
            }

            fn reaped(&self) -> bool {
                self.exit.reaped()
            }

            fn key(&self) -> Option<&AdoptionKey> {
                None
            }

            fn wait(&mut self) -> io::Result<()> {
                self.exit.wait()
            }
        }

        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn owned test child");
        let held = HeldRetry {
            exit: crate::stream::ExitSlot::Held {
                child,
                reap: crate::stream::HeldReap::default(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                exit_group: None,
            },
            signals: Cell::new(0),
        };
        let mut children = BTreeMap::from([(10, held)]);

        let first = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert!(ContainmentUnproven::is(&first), "{first}");
        assert_eq!(children[&10].signals.get(), 1);

        let retry = shutdown_children(&mut children, None, NO_GRACE).unwrap_err();
        assert!(ContainmentUnproven::is(&retry), "{retry}");
        assert_eq!(
            children[&10].signals.get(),
            1,
            "a successfully waited Held child kept stale reservation authority"
        );
    }
}
