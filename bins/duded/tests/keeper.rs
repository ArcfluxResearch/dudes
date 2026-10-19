//! `duded keeper`: the hidden mode that ends the process group it joined once its standard input
//! reaches end-of-file. Every case runs the built binary, because the behaviour is a pair of
//! kernel facts (a pipe closing, a group receiving `SIGKILL`) that no in-process fake can show.

#![cfg(unix)]
#![allow(
    clippy::disallowed_types,
    reason = "the test runs the built daemon binary; host_seam.rs separately guards src/"
)]

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

const DUDED: &str = env!("CARGO_BIN_EXE_duded");
const SETTLE: Duration = Duration::from_millis(500);
const BOUND: Duration = Duration::from_secs(5);

/// A stand-in agent: a process that leads a group of its own and never exits by itself.
fn leader() -> Child {
    Command::new("sleep")
        .arg("120")
        .process_group(0)
        .spawn()
        .expect("sleep starts")
}

fn group_of(child: &Child) -> i32 {
    i32::try_from(child.id()).expect("pid fits")
}

/// A keeper that joined `group`, with a pipe the test holds the write end of.
fn keeper_in(group: i32) -> Child {
    Command::new(DUDED)
        .arg("keeper")
        .env_clear()
        .process_group(group)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the keeper starts")
}

fn wait_for(child: &mut Child, bound: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pid(raw: i32) -> Pid {
    Pid::from_raw(raw).expect("nonzero")
}

/// Ends the group when a failed assertion unwinds, so a red test leaves nothing running.
struct GroupGuard(i32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let _ = kill_process_group(pid(self.0), Signal::KILL);
    }
}

#[test]
fn end_of_file_ends_the_whole_group_at_once() {
    let mut agent = leader();
    let group = group_of(&agent);
    let _guard = GroupGuard(group);
    let mut keeper = keeper_in(group);
    std::thread::sleep(SETTLE);
    assert!(
        agent.try_wait().unwrap().is_none(),
        "the agent was ended early"
    );
    assert!(
        keeper.try_wait().unwrap().is_none(),
        "the keeper left early"
    );

    let stdin = keeper.stdin.take().expect("the keeper's pipe");
    let closed = Instant::now();
    drop(stdin);

    let keeper_status = wait_for(&mut keeper, BOUND).expect("the keeper ended");
    let agent_status = wait_for(&mut agent, BOUND).expect("the agent ended");
    let elapsed = closed.elapsed();
    assert_eq!(
        keeper_status.signal(),
        Some(9),
        "the keeper ends by SIGKILL"
    );
    assert_eq!(agent_status.signal(), Some(9), "the agent ends by SIGKILL");
    assert!(
        elapsed < BOUND,
        "the group outlived its pipe by {elapsed:?}"
    );
    assert_eq!(
        test_kill_process_group(pid(group)),
        Err(Errno::SRCH),
        "something is still in the group"
    );
}

#[test]
fn a_pipe_already_closed_when_the_keeper_starts_ends_the_group_at_once() {
    // The keeper's parent is a shell that exits before the keeper execs, so the keeper starts after
    // its "daemon" is gone: its parent is the reaper and the pipe it reads has no writer.
    let mut agent = leader();
    let group = group_of(&agent);
    let _guard = GroupGuard(group);
    let mut shell = Command::new("sh")
        .args([
            "-c",
            "( sleep 0.3; exec \"$0\" keeper ) <&0 >/dev/null 2>&1 &",
            DUDED,
        ])
        .env_clear()
        .process_group(group)
        .stdin(Stdio::piped())
        .spawn()
        .expect("sh starts");
    drop(shell.stdin.take());
    wait_for(&mut shell, BOUND).expect("the shell left");

    let agent_status = wait_for(&mut agent, BOUND).expect("the agent was not ended");
    assert_eq!(agent_status.signal(), Some(9));
}

/// Every signal a stray sender could use against a group, which the keeper must outlive.
const STRAYS: [Signal; 5] = [
    Signal::TERM,
    Signal::HUP,
    Signal::INT,
    Signal::QUIT,
    Signal::USR1,
];

/// Wait until the keeper at `pid` blocks the signals, where the platform lets another process see
/// that, and otherwise wait out a bound that is generous for a cold start under load.
fn await_blocking(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + BOUND;
        while Instant::now() < deadline {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            let blocked = status
                .lines()
                .find_map(|line| line.strip_prefix("SigBlk:"))
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
                .unwrap_or(0);
            // SIGHUP is bit 0 and SIGTERM bit 14.
            if blocked & 1 == 1 && blocked & (1 << 14) != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the keeper never blocked its signals");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn send_strays_and_check(keeper: &mut Child, agent: &mut Child, group: i32) {
    for signal in STRAYS {
        kill_process_group(pid(group), signal).expect("the group takes the signal");
    }
    // The stand-in agent dies of the first, as a real one would.
    assert!(wait_for(agent, BOUND).is_some());
    std::thread::sleep(SETTLE);
    let status = keeper.try_wait().unwrap();
    assert!(
        status.is_none(),
        "a stray signal removed the keeper: {status:?}"
    );

    drop(keeper.stdin.take());
    assert_eq!(
        wait_for(keeper, BOUND).expect("the keeper ended").signal(),
        Some(9)
    );
}

#[test]
fn the_keeper_blocks_stray_stop_signals_itself() {
    let mut agent = leader();
    let group = group_of(&agent);
    let _guard = GroupGuard(group);
    let mut keeper = keeper_in(group);
    await_blocking(keeper.id());
    send_strays_and_check(&mut keeper, &mut agent, group);
}

#[test]
fn a_keeper_started_with_signals_blocked_keeps_them_blocked() {
    // What the daemon does: the spawning thread blocks everything around the spawn, so the keeper
    // has no unprotected moment between exec and its own block.
    let mut agent = leader();
    let group = group_of(&agent);
    let _guard = GroupGuard(group);
    let before = SigSet::all()
        .thread_swap_mask(SigmaskHow::SIG_SETMASK)
        .expect("block");
    let mut keeper = keeper_in(group);
    before.thread_set_mask().expect("restore");
    send_strays_and_check(&mut keeper, &mut agent, group);
}

#[test]
fn run_by_hand_from_a_shell_signals_nothing() {
    // A script without job control: the keeper inherits the shell's group, so its parent's group is
    // its own. If it signalled anything, the shell would die before it printed.
    let output = Command::new("sh")
        .args([
            "-c",
            "\"$0\" keeper </dev/null 2>/dev/null; echo \"exit=$?\"",
            DUDED,
        ])
        .env_clear()
        .process_group(0)
        .stdin(Stdio::null())
        .output()
        .expect("sh runs");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "exit=1\n");

    // A single command typed into an interactive shell leads a group of its own.
    let status = Command::new(DUDED)
        .arg("keeper")
        .env_clear()
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("keeper runs");
    assert_eq!(status.code(), Some(1), "{status:?}");
}

#[test]
fn the_keeper_takes_no_arguments() {
    let status = Command::new(DUDED)
        .args(["keeper", "1234"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("keeper runs");
    assert_eq!(status.code(), Some(1), "{status:?}");
}
