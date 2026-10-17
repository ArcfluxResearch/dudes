//! What Linux and macOS do the same way: make every child a process-group leader, signal a
//! process or a group, and ask whether a group still has anything in it.
//!
//! No `unsafe` in this module. Every syscall on this path is reached through `rustix`'s safe
//! wrappers, and the group is established by [`CommandExt::process_group`], which is safe. The one
//! `pre_exec` closure `prepare` installs, which clears the child's signal mask, lives in
//! `sigmask.rs`, the module the unsafe grant names for it.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

use crate::kill::Sig;

/// What a signalling syscall established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignalResult {
    /// The kernel accepted the signal for at least the target it reports success for.
    Accepted,
    /// The target exists and this call signalled nothing in it.
    ///
    /// Only a *group* sweep produces this, and only through [`classify_group_signal`]: `killpg`
    /// answers `EPERM` when it found no member it would signal, which is a statement about a set
    /// rather than about permission. Never absence — the group is there — and never acceptance,
    /// because nothing took the signal.
    ///
    /// **Deliberately not a near-twin of [`Presence::Unsignallable`].** That one says what a
    /// *query* found; this one says what a *delivery* reached. A glossary pair one letter apart is
    /// how two facts get read as one.
    NothingSignalled,
    /// The kernel found no corresponding process or group.
    Absent,
}

/// What a signal-zero query established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    /// The target exists and this process may signal it.
    Present,
    /// The target exists and holds nothing this host may signal.
    ///
    /// Deliberately says nothing about groups, because [`classify_presence`] says nothing about
    /// groups: it serves the direct-process query too, and a meaning that named only groups would
    /// be untrue for one of its own producers. Never absence — a group of live processes belonging
    /// to another user answers exactly this way — and never quiet.
    Unsignallable,
    /// The kernel found no corresponding process or group.
    Absent,
}

/// Classify a **direct**, one-named-process signalling result without turning a permission failure
/// into absence.
///
/// This is the direct form's classifier and nothing else's: `signal_leader` on each platform. Its
/// `EPERM` is `cansignal` failing for one named process — a real authorization refusal about a
/// process this host may not touch — and stays a concrete error. A group sweep's `EPERM` means something different and has its own
/// classifier: [`classify_group_signal`].
pub(crate) fn classify_signal(result: rustix::io::Result<()>) -> io::Result<SignalResult> {
    match result {
        Ok(()) => Ok(SignalResult::Accepted),
        Err(Errno::SRCH) => Ok(SignalResult::Absent),
        Err(errno) => Err(errno.into()),
    }
}

/// Classify a **delivered group sweep**, whose `EPERM` is a fact about the set and not a refusal.
///
/// `killpg` walks the group and signals what it may; `EPERM` is what it returns when that walk
/// ended with nothing signalled — Darwin's `nfound == 0` rule. The commonest such
/// group is one whose only member is this host's own child in the window between its exit and its
/// reap, and calling that a failed kill is how a child this host successfully killed came to be
/// reported as still owned.
///
/// **Form-specific, where [`classify_presence`] is deliberately form-blind, and the asymmetry is
/// measured rather than stylistic**: both *query* forms' `EPERM` carry
/// the same fact, so one classifier can serve them; the two *delivered* forms do not, so a
/// form-blind classifier would have to pick one meaning and be wrong about the other.
///
/// Two facts already qualify the answer before this runs, so the group is never a stranger's: the
/// guard admitted this target, and — on `plat::signal` — the direct leader answered first, so a
/// foreign target's `EPERM` propagates from the leader and never reaches here.
pub(crate) fn classify_group_signal(result: rustix::io::Result<()>) -> io::Result<SignalResult> {
    match result {
        Ok(()) => Ok(SignalResult::Accepted),
        Err(Errno::PERM) => Ok(SignalResult::NothingSignalled),
        Err(Errno::SRCH) => Ok(SignalResult::Absent),
        Err(errno) => Err(errno.into()),
    }
}

/// Classify signal zero without turning a failed permission check into absence.
///
/// `EPERM` is an answer about the target and not a failure to ask: the kernel found the target and
/// declined to count anything in it as signallable. The common case is the host's own child in the
/// moment between its exit and its reap; the other case is a target this host genuinely may not
/// touch. Neither is absence, and neither is quiet, so both take the same variant and the poll
/// keeps running until something says `ESRCH`.
///
/// `EPERM` from a *delivered* signal is untouched by this and stays a concrete error — see
/// [`classify_signal`], which is the reason the two classifiers are separate functions.
pub(crate) fn classify_presence(result: rustix::io::Result<()>) -> io::Result<Presence> {
    match result {
        Ok(()) => Ok(Presence::Present),
        Err(Errno::PERM) => Ok(Presence::Unsignallable),
        Err(Errno::SRCH) => Ok(Presence::Absent),
        Err(errno) => Err(errno.into()),
    }
}

/// Put the child in a process group of its own, with the child as leader, and give it an empty
/// signal mask.
///
/// `process_group(0)` establishes the child's initial group ID as its PID; a later live-process
/// lookup can become unavailable after exit, even before reap. A signal to that group reaches its
/// remaining members.
/// The point of doing it here rather than assuming it is that assuming it is how a supervisor ends
/// up sweeping a group that does not exist: a child spawned without this inherits *its parent's*
/// group, and `killpg(child_pid, …)` then names no group at all, fails with `ESRCH`, and every
/// descendant is orphaned.
pub(crate) fn prepare(command: &mut Command) {
    command.process_group(0);
    crate::sigmask::clear_in_child(command);
}

/// A pid as the signalling calls want it. `None` for 0 and for values that do not fit, both of
/// which name something other than one process — 0 is "my whole group", and the host never means
/// that.
pub(crate) fn as_pid(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

/// The two signals, as signals.
pub(crate) const fn signal_of(sig: Sig) -> Signal {
    match sig {
        Sig::Term => Signal::TERM,
        Sig::Kill => Signal::KILL,
    }
}

/// `Some(pid)` when the process at `pid` leads its own group, `None` otherwise.
///
/// The check is the whole function. A supervisor that signals `-pid` without it is asserting
/// something about a process it did not spawn, and when the assertion is false the negated
/// identifier names a *different* group — possibly the daemon's own. So a group is swept only when
/// the host has established that this identifier is the group's name.
pub(crate) fn group_of_leader(pid: u32) -> Option<Pid> {
    let pid = as_pid(pid)?;
    let group = rustix::process::getpgid(Some(pid)).ok()?;
    (group == pid).then_some(group)
}

/// Signal an addressable process group while preserving accepted, nothing-signalled, absent and
/// unavailable.
pub(crate) fn signal_group(group: Option<Pid>, sig: Sig) -> io::Result<Option<SignalResult>> {
    let Some(group) = group else {
        return Ok(None);
    };
    let result = rustix::process::kill_process_group(group, signal_of(sig));
    // The delivered site's half of the reap-window seam, and the reason it exists here rather than
    // only at the query: without the group-sweep fix the sweep fails *before* any
    // presence query runs, so the query's hook never fires and a test holding the window open would
    // sit for the failsafe. Keyed on the errno and not on the classification below, so the same
    // test compiles and runs identically with or without the group-sweep fix — `reap_hold`'s own doctrine.
    #[cfg(all(test, target_os = "macos"))]
    if matches!(result, Err(Errno::PERM)) {
        crate::reap_hold::release_for_group_signal(
            u32::try_from(group.as_raw_nonzero().get()).unwrap_or(0),
        );
    }
    classify_group_signal(result).map(Some)
}

/// Whether there is anything left that this host could signal.
///
/// Deliberately a *query* and not a wait: `waitpid` would reap, and reaping the leader releases
/// the identifier the group is named by while descendants may still be inside it. Signal 0 runs
/// the error checks and delivers nothing, so it answers the question without changing anything.
///
/// `Ok` means something in there is present and signallable; `ESRCH` means nothing is there;
/// `EPERM` means something is there that this host cannot signal, which is not quiet either.
pub(crate) fn quiet(group: Option<Pid>, pid: u32) -> bool {
    quiet_checked(group, pid).unwrap_or(false)
}

/// [`quiet`] with a query that could not be answered at all preserved as an error, so the
/// failed-spawn path never reads one as absence evidence.
///
/// The query and the decision, separated: this asks the platform, [`quiet_from`] decides.
pub(crate) fn quiet_checked(group: Option<Pid>, pid: u32) -> io::Result<bool> {
    Ok(quiet_from(presence_checked(group, pid)?))
}

/// Whether a [`Presence`] means there is nothing left in this target to signal.
///
/// **Its own function because the decision is the part worth pinning, and a decision behind a
/// syscall cannot be pinned.** Inline in [`quiet_checked`] this reads as a formality — three
/// variants, two answers — and the whole crate suite stays green with `Unsignallable` moved to the
/// other side of it. That mutant is neither hypothetical nor small: it is exactly the permission
/// the host considered and refused, and taking it releases an ownership row over a group that may
/// still hold a live process of another user, with a successful-looking result and nothing logged.
///
/// An exhaustive `match` for a second reason: a `matches!` against one variant keeps compiling when
/// a variant is added and decides for it silently, which is how [`Presence::Unsignallable`] would
/// have been classified by nobody.
///
/// The rule, stated once and reachable without a process: **`Absent` is the only quiet answer.** A
/// target that exists is never quiet, however little this host can do to it.
pub(crate) const fn quiet_from(presence: Presence) -> bool {
    match presence {
        Presence::Absent => true,
        Presence::Present | Presence::Unsignallable => false,
    }
}

/// Query the addressable group when one exists, otherwise the direct process.
pub(crate) fn presence_checked(group: Option<Pid>, pid: u32) -> io::Result<Presence> {
    let result = match group {
        Some(group) => rustix::process::test_kill_process_group(group),
        None => match as_pid(pid) {
            Some(pid) => rustix::process::test_kill_process(pid),
            None => return Ok(Presence::Absent),
        },
    };
    // The one place a held-open reap window can be closed, and it is closed by this query having
    // *seen* the window rather than by a test having guessed at it. Keyed on the errno and not on
    // the classification below, so the same test runs unchanged either side of that change.
    #[cfg(all(test, target_os = "macos"))]
    if group.is_some() && matches!(result, Err(Errno::PERM)) {
        crate::reap_hold::release_for(pid);
    }
    classify_presence(result)
}

#[cfg(test)]
mod tests {
    use super::{
        Presence, SignalResult, classify_group_signal, classify_presence, classify_signal,
        quiet_from,
    };
    use rustix::io::Errno;

    /// `Absent` is the only quiet answer, and the arm that matters is the middle one.
    ///
    /// Without this, moving `Unsignallable` to the quiet side changes nothing any test can see:
    /// measured on the whole crate, 111 tests run and 111 passed under exactly that mutant. The
    /// real kill path settles sooner and reports success, so the tests that drive it agree with it.
    /// This is the assertion that does not.
    #[test]
    fn only_absence_is_quiet() {
        assert!(
            quiet_from(Presence::Absent),
            "absence stopped being the quiet answer"
        );
        assert!(
            !quiet_from(Presence::Present),
            "a present target was called quiet"
        );
        assert!(
            !quiet_from(Presence::Unsignallable),
            "an unsignallable target was called quiet, which releases the ownership row over a \
             group that may still hold a live process of another user — the permission the host \
             considered and refused"
        );
    }

    /// Both classifiers, all four answers each, and the difference between them is the whole test.
    ///
    /// A *delivered* signal's `EPERM` is a concrete error and stays one. A *query*'s `EPERM` is an
    /// answer about the target — [`Presence::Unsignallable`] — and what this pins is that it is
    /// never [`Presence::Absent`]: absence is `ESRCH` and nothing else, on either side.
    #[test]
    fn eperm_is_never_absence() {
        assert_eq!(classify_signal(Ok(())).unwrap(), SignalResult::Accepted);
        assert_eq!(
            classify_signal(Err(Errno::SRCH)).unwrap(),
            SignalResult::Absent
        );
        assert_eq!(
            classify_signal(Err(Errno::PERM))
                .unwrap_err()
                .raw_os_error(),
            Some(Errno::PERM.raw_os_error())
        );
        assert_eq!(
            classify_signal(Err(Errno::INTR))
                .unwrap_err()
                .raw_os_error(),
            Some(Errno::INTR.raw_os_error())
        );

        // The group form, whose `EPERM` is the set's answer rather than a refusal. Every other
        // errno still errors, and `ESRCH` is still the only absence — collapsing those is the
        // failure mode the group-sweep change exists to avoid, so both directions are asserted here.
        assert_eq!(
            classify_group_signal(Ok(())).unwrap(),
            SignalResult::Accepted
        );
        assert_eq!(
            classify_group_signal(Err(Errno::SRCH)).unwrap(),
            SignalResult::Absent
        );
        assert_eq!(
            classify_group_signal(Err(Errno::PERM)).unwrap(),
            SignalResult::NothingSignalled,
            "a group sweep that signalled nothing is not a failed kill"
        );
        assert_ne!(
            classify_group_signal(Err(Errno::PERM)).unwrap(),
            SignalResult::Absent,
            "a group that signalled nothing was called absent"
        );
        assert_eq!(
            classify_group_signal(Err(Errno::INTR))
                .unwrap_err()
                .raw_os_error(),
            Some(Errno::INTR.raw_os_error())
        );

        assert_eq!(classify_presence(Ok(())).unwrap(), Presence::Present);
        assert_eq!(
            classify_presence(Err(Errno::SRCH)).unwrap(),
            Presence::Absent
        );
        assert_eq!(
            classify_presence(Err(Errno::PERM)).unwrap(),
            Presence::Unsignallable,
            "a query's permission denial is an answer about the target, not a failed query"
        );
        assert_ne!(
            classify_presence(Err(Errno::PERM)).unwrap(),
            Presence::Absent,
            "a permission denial became absence"
        );
        assert_eq!(
            classify_presence(Err(Errno::INTR))
                .unwrap_err()
                .raw_os_error(),
            Some(Errno::INTR.raw_os_error())
        );
    }
}
