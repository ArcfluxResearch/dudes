//! The one place the host runs code between `fork` and `exec`.
//!
//! The daemon blocks `SIGTERM` and `SIGINT` on every thread, and a child inherits the mask of the
//! thread that spawns it. Left alone, every child would start unable to receive the polite stop,
//! and each stop would wait out the whole grace before `SIGKILL`. So each child clears its mask
//! before it execs.

#![allow(unsafe_code)]

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use nix::sys::signal::SigSet;

/// Start `command`'s child with no signals blocked, whatever the spawning thread blocks.
pub(crate) fn clear_in_child(command: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls
    // are allowed. It makes one `pthread_sigmask` call with an empty set, which allocates nothing,
    // takes no lock and touches no memory the parent shares.
    unsafe {
        command.pre_exec(|| SigSet::empty().thread_set_mask().map_err(io::Error::from));
    }
}
