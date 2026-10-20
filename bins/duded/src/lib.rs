//! `duded`: the Dudes daemon, as a library so its ordering can be asserted.
//!
//! # What this crate is
//!
//! The process that owns the journal, runs the pure loop, and turns the supervisor's effects into
//! real work. Every other part of the system produces a piece — the machine decides, the host owns
//! children, the transport carries calls, the policy is a function — and this is where they are
//! assembled and where the loop actually runs for the first time.
//!
//! - [`boot`] — the seven core startup steps and the optional prepared/ready Floor steps.
//! - [`classify`] — the two ports the peer-classification table is decided over, which `dudes-rpc` declares
//!   and, by the layering rule, cannot implement.
//! - [`config`] — the restart-loaded operator budget parsed before persistent or child work.
//! - [`floor`] — daemon-local assembly of replay-derived Floor agent and board slices.
//! - [`runtime`] — the decide loop, the effect executor, and the exit sequence.
//! - [`ports`] — the daemon's side of the transport's `Control` and `JournalRead`, and the handle
//!   every other thread reaches the loop through.
//! - [`predicate`] — the one-shot runner seam, the daemon-owned environment, and `argv[0]`
//!   resolution.
//! - [`lock`] — invariant 1's single-writer lock.
//! - [`socket`] — reclaiming a dead run's files, and the `(st_dev, st_ino)` re-comparison.
//! - [`startup`] — the two step orders and the recorder that makes them assertable.
//!
//! # Nothing here names a concrete child owner
//!
//! The code alone cannot establish that the daemon reaches the host *through the trait*: a daemon
//! holding a concrete `RealHost` and calling the inherent method would satisfy invariant 10 while
//! making the trait change pointless. So every type here is generic over [`dudes_supervisor::ProcessHost`] and over
//! [`predicate::PredicateRunner`], `src/main.rs` is the only file in the package that names the
//! crate implementing them, and `tests/host_seam.rs` is the scan that keeps it that way.
//!
//! # And nothing here creates a process
//!
//! `bins/duded/clippy.toml` disallows `std::process::Command` outright, with no
//! allow-listed module, because every child in this process — an agent's, and a goal predicate's —
//! is created by `dudes-host`. An allow-listed module would be a permanent blind spot exactly its
//! own size.
//!
//! # Why there is a library target at all
//!
//! Many of this daemon's required behaviours are assertions about internal ordering: the startup
//! steps, exactly one child after a crash, `shutdown` once per exit path and after the
//! journal flush. A bin-only package cannot be reached by an integration test. With a lib target
//! `tests/` reaches the logic directly, and still reaches the real binary through
//! `env!("CARGO_BIN_EXE_duded")` for the exits that need a real process and a real signal.

#[cfg(test)]
extern crate self as duded;

mod agent_bin;
pub mod baseline;
mod board;
pub mod boot;
mod chat_launch;
pub mod children;
pub mod classify;
mod closing;
mod codex_profile;
mod condense_output;
pub mod config;
pub mod diff_capture;
pub mod enrollment_reload;
/// The end-to-end gate's crash arms, and nothing else. Absent from every build that does not ask for it —
/// `cargo build --release --workspace` does not.
#[cfg(feature = "fault-injection")]
pub mod fault;
#[doc(hidden)]
pub mod floor;
pub mod floor_actions;
pub mod floor_answer;
pub mod floor_chat;
pub mod floor_control;
pub mod floor_diff;
pub mod floor_gateway;
pub mod floor_git;
pub mod floor_goal;
pub mod floor_look;
pub mod floor_memory;
mod floor_peer;
mod floor_port;
pub mod floor_recovery;
pub mod floor_rename;
pub mod floor_retire;
pub mod floor_session;
mod floor_settings;
pub mod floor_settings_edit;
#[doc(hidden)]
pub mod floor_stream;
pub mod floor_switches;
pub mod floor_terminal;
pub mod floor_ticket;
mod floor_websocket;
pub mod git_read;
pub mod hooks;
mod instruction_sample;
#[cfg(unix)]
pub mod keeper;
pub mod local_day;
pub mod lock;
mod mailbox;
pub mod output;
mod pi_profile;
pub mod ports;
pub mod predicate;
pub mod program;
pub mod provider_check;
pub mod pty_output;
mod reserve;
pub mod runtime;
pub mod settings;
mod shutdown_receipt;
pub mod sign_in;
pub mod socket;
pub mod startup;
mod ticket_recovery;
mod work;
pub mod worktree;

pub use boot::{boot, boot_with_options};

#[doc(hidden)]
pub use chat_launch::BrowserLaunchPause;
pub use children::{ChildEnds, ChildInput, ChildTurn};
pub use ports::{ExitCause, Handle};
pub use runtime::Daemon;
pub use settings::{Seams, Settings};
pub use startup::{Recorder, StartupStep, TeardownStep, Witness};

use dudes_supervisor::{Clock, ProcessHost, Rng};

use crate::predicate::PredicateRunner;

/// Boot and serve, in one call: what `main` does.
///
/// # Errors
///
/// Any startup step's failure, or the loop's. On the second the exit sequence has still run in
/// full — see [`Daemon::run`].
pub fn run<H, P, C, R>(settings: &Settings, seams: Seams<H, P, C, R>) -> anyhow::Result<ExitCause>
where
    H: ProcessHost,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: Rng,
{
    boot(settings, seams)?.run()
}

mod goal_replacement;
mod ticket;
mod ticket_index;
mod ticket_queue;

/// The name a ticket's goal records for its check, for tests that build a ticket's start.
#[doc(hidden)]
#[must_use]
pub fn ticket_goal_check(
    check: &dudes_proto::TicketCheck,
) -> Option<dudes_proto::CompletionCheckRef> {
    ticket_index::goal_check(check)
}
