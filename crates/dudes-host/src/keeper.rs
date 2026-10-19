//! The keeper the daemon starts beside each agent, so that the agent's process group ends with the
//! daemon.
//!
//! On Unix a process group selects processes; it does not bound their lifetime. The keeper is a
//! second process in the agent's group that blocks reading a pipe whose write end only this host
//! holds. The kernel closes that write end however the daemon dies, the keeper reads end-of-file,
//! and it sends `SIGKILL` to its own group. The agent itself is spawned exactly as before and
//! stays the host's direct child and its group's leader, so nothing the host reads about it moves.
//!
//! The keeper is the daemon's own executable run in a hidden mode. This module only starts it, joins
//! it to the agent's group and owns it; what it does lives with the daemon.

use std::io;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::process::Signal;

/// The write end of a running keeper's pipe.
///
/// Holding it keeps the agent's group alive; dropping it ends the group. Whoever owns the agent
/// therefore owns this, for exactly as long, and no path that leaves the agent running may drop it.
#[derive(Debug)]
pub(crate) struct Lifeline(
    #[allow(
        dead_code,
        reason = "held for its Drop: closing the pipe is the signal"
    )]
    ChildStdin,
);

/// Start a keeper that joins the group led by `leader`.
///
/// The spawning thread blocks every signal around the spawn, because a child keeps its parent
/// thread's mask across exec. That leaves the keeper no moment between exec and its own block in
/// which a stray signal sent to the group could remove it.
///
/// The keeper's `Child` goes to a thread that waits on it, so it is reaped whenever it ends: when
/// the agent's exit sweep kills the group, or when the lifeline drops. An unreaped keeper would
/// read as a live member of the group to the stop's quiet check.
///
/// # Errors
///
/// The keeper could not be started or handed to its reaper. Nothing is left running by it.
pub(crate) fn start(executable: &Path, leader: u32) -> io::Result<Lifeline> {
    let group = i32::try_from(leader)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a process group"))?;
    let mut command = Command::new(executable);
    command
        .arg0("duded")
        .arg("keeper")
        .env_clear()
        .current_dir("/")
        .process_group(group)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let previous = SigSet::all().thread_swap_mask(SigmaskHow::SIG_SETMASK)?;
    let spawned = crate::spawn_serialized(&mut command);
    let restored = previous.thread_set_mask();
    let mut child = spawned?;
    if let Err(error) = restored {
        end(child);
        return Err(error.into());
    }
    let Some(pipe) = child.stdin.take() else {
        end(child);
        return Err(io::Error::other("the keeper has no standard input"));
    };

    // The child travels over a channel because a closure that captured it would drop it on a failed
    // thread start, and dropping a `Child` neither signals nor reaps it.
    let (handover, received) = mpsc::channel::<Child>();
    match thread::Builder::new()
        .name(format!("dudes-keeper-{leader}"))
        .spawn(move || {
            if let Ok(mut child) = received.recv()
                && let Ok(status) = child.wait()
            {
                report_unexpected_end(leader, status);
            }
        }) {
        Ok(_) => match handover.send(child) {
            Ok(()) => Ok(Lifeline(pipe)),
            Err(returned) => {
                end(returned.0);
                Err(io::Error::other(
                    "the keeper's reaper ended before the keeper",
                ))
            }
        },
        Err(error) => {
            end(child);
            Err(error)
        }
    }
}

/// A keeper ends the group, itself included, with `SIGKILL`, and so does the host's own sweep, so
/// that is the only ending that is silent. Any other ending leaves the agent running with nothing
/// to end it when the daemon dies.
fn report_unexpected_end(leader: u32, status: ExitStatus) {
    if status.signal() != Some(Signal::KILL.as_raw()) {
        tracing::warn!(
            agent_pid = leader,
            %status,
            "the keeper ended without ending the agent's group, so the agent no longer ends with the daemon"
        );
    }
}

fn end(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}
