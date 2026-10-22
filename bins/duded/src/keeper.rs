//! The hidden `duded keeper` mode: it ends an agent's process group when the daemon dies.
//!
//! On macOS and Linux a process group is a way to select processes, not a lifetime. A daemon that
//! dies hard (a crash, an out-of-memory kill, `kill -9`) leaves its agents running with nobody
//! watching them, and a restarted daemon that cannot find one starts a second copy of the same
//! task beside it. The daemon therefore starts one keeper beside each agent. The keeper joins the
//! agent's group and blocks reading its standard input, a pipe whose write end only the daemon
//! holds. The kernel closes that write end however the daemon dies, so the keeper reads
//! end-of-file within milliseconds and sends `SIGKILL` to its own group, which ends the keeper too.
//!
//! The mode takes no pid, group or command from anyone. Its authority is its own group, and it
//! uses it only when that group is demonstrably not its parent's.

use std::io;

use anyhow::Context as _;
use nix::sys::signal::SigSet;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, getpgid, getpgrp, getpid, getppid, kill_process_group};

use crate::ExitCause;

/// What the keeper learned about the process that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    /// The kernel reports no parent, which happens when the parent lives outside this process's pid
    /// namespace. A keeper the daemon spawns directly is never in that position.
    Unnamed,
    /// The parent was named and is already gone, so a reaper adopted this process before the group
    /// could be read. That is the daemon having died, which is what the keeper exists to answer.
    Gone,
    /// The parent's process group.
    Group(Pid),
}

/// Why the keeper refuses to signal anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The parent cannot be named, so the group comparison cannot be made.
    NoParent,
    /// This process shares its parent's group: the join did not happen, or someone ran the mode by
    /// hand from a shell. Ending the group would end the daemon or the shell.
    SharesParentsGroup,
    /// This process leads its own group. A keeper joins an existing group and so never leads one;
    /// a process that does was started by hand.
    LeadsItsOwnGroup,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoParent => "the parent process cannot be named, so nothing is signalled",
            Self::SharesParentsGroup => {
                "this process shares its parent's process group, so nothing is signalled"
            }
            Self::LeadsItsOwnGroup => {
                "this process leads its own process group, so nothing is signalled"
            }
        })
    }
}

impl std::error::Error for Refusal {}

/// Whether a process with this identity may end its own group.
///
/// A pure function of what the kernel reported, so every refusal is testable without a process.
///
/// # Errors
///
/// The reason the group must be left alone.
pub fn judge(own_pid: Pid, own_group: Pid, parent: Parent) -> Result<(), Refusal> {
    if own_pid == own_group {
        return Err(Refusal::LeadsItsOwnGroup);
    }
    match parent {
        Parent::Unnamed => Err(Refusal::NoParent),
        Parent::Group(group) if group == own_group => Err(Refusal::SharesParentsGroup),
        Parent::Gone | Parent::Group(_) => Ok(()),
    }
}

fn read_parent() -> io::Result<Parent> {
    let Some(parent) = getppid() else {
        return Ok(Parent::Unnamed);
    };
    match getpgid(Some(parent)) {
        Ok(group) => Ok(Parent::Group(group)),
        Err(Errno::SRCH) => Ok(Parent::Gone),
        Err(errno) => Err(errno.into()),
    }
}

/// Run the keeper. Returns only when it refuses or fails: a keeper that does its work ends in the
/// `SIGKILL` it sends its own group.
///
/// # Errors
///
/// A refusal, or a failure to read the identity it decides on or to send the signal.
pub fn run() -> anyhow::Result<ExitCause> {
    // Every blockable signal, on the only thread, before anything else: a stray `SIGTERM`, `SIGINT`
    // or `SIGHUP` sent to the agent's group must not remove the process that guards it.
    SigSet::all()
        .thread_block()
        .context("could not block signals")?;
    let group = getpgrp();
    judge(
        getpid(),
        group,
        read_parent().context("could not read the parent's process group")?,
    )?;
    // End-of-file is the only event that matters. Whatever arrives on the pipe is ignored, and a
    // read error ends the wait the same way: the pipe is gone, and so is whatever held it.
    let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
    kill_process_group(group, Signal::KILL).context("could not signal the process group")?;
    Ok(ExitCause::Ended)
}

#[cfg(test)]
mod tests {
    use super::{Parent, Refusal, judge};
    use rustix::process::Pid;

    fn pid(raw: i32) -> Pid {
        Pid::from_raw(raw).expect("a nonzero pid")
    }

    #[test]
    fn a_keeper_in_another_group_than_its_parent_may_act() {
        assert_eq!(judge(pid(20), pid(10), Parent::Group(pid(5))), Ok(()));
    }

    #[test]
    fn a_keeper_whose_daemon_is_gone_may_act() {
        assert_eq!(judge(pid(20), pid(10), Parent::Gone), Ok(()));
    }

    #[test]
    fn a_keeper_in_its_parents_group_refuses() {
        assert_eq!(
            judge(pid(20), pid(10), Parent::Group(pid(10))),
            Err(Refusal::SharesParentsGroup)
        );
    }

    #[test]
    fn a_keeper_that_cannot_name_its_parent_refuses() {
        assert_eq!(
            judge(pid(20), pid(10), Parent::Unnamed),
            Err(Refusal::NoParent)
        );
    }

    #[test]
    fn a_process_that_leads_its_group_refuses_even_with_a_departed_parent() {
        assert_eq!(
            judge(pid(20), pid(20), Parent::Gone),
            Err(Refusal::LeadsItsOwnGroup)
        );
        assert_eq!(
            judge(pid(20), pid(20), Parent::Group(pid(5))),
            Err(Refusal::LeadsItsOwnGroup)
        );
    }
}
