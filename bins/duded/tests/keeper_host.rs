//! The host's keeper, end to end on a real operating system: `RealHost` starts the built `duded` as
//! a keeper beside each agent, and the tests kill, stop and crash around it.
//!
//! The crash cases run the host in a helper process (this test binary again, selected by an
//! environment variable) and `SIGKILL` it, because "the daemon died" is a kernel event that an
//! in-process drop cannot stand in for.

#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(
    clippy::disallowed_types,
    reason = "the test runs the built daemon binary and real agents; host_seam.rs separately guards src/"
)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use dudes_host::RealHost;
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_supervisor::ProcessHost as _;
use dudes_supervisor::host::{
    AdoptionKey, ContainmentUnproven, KillOutcome, Liveness, SpawnResult,
};

const DUDED: &str = env!("CARGO_BIN_EXE_duded");
const HELPER_DIR: &str = "KEEPER_HOST_HELPER_DIR";
const HELPER_SCENARIO: &str = "KEEPER_HOST_HELPER_SCENARIO";
const BOUND: Duration = Duration::from_secs(10);

fn keeping_host(grace: Duration) -> RealHost {
    RealHost::with_grace(grace).with_keeper(PathBuf::from(DUDED))
}

/// `shutdown` ends every child. Its answer for a group it ended is "whole absent but containment
/// unproven" on a host that cannot prove a group complete, which the daemon accepts as well.
fn shut_down(host: &mut RealHost) {
    if let Err(error) = host.shutdown() {
        assert!(ContainmentUnproven::is(&error), "{error}");
    }
}

fn script(body: &str, cwd: &Path, stdin: LaunchInput) -> LaunchSpec {
    LaunchSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), body.into()],
        env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
        cwd: cwd.to_path_buf(),
        stdin,
    }
}

fn launch(host: &mut RealHost, body: &str, cwd: &Path) -> AdoptionKey {
    launch_with(host, body, cwd, LaunchInput::Null)
}

fn launch_with(host: &mut RealHost, body: &str, cwd: &Path, stdin: LaunchInput) -> AdoptionKey {
    match host.spawn(&script(body, cwd, stdin), &ManagedInstall::default()) {
        SpawnResult::Spawned(key) => key,
        SpawnResult::NotLaunched { reason } => panic!("not launched: {reason}"),
        SpawnResult::CleanupUnconfirmed { reason } => panic!("cleanup unconfirmed: {reason}"),
    }
}

/// One row of the process table.
struct Row {
    pid: u32,
    group: u32,
    state: String,
    command: String,
}

fn table() -> Vec<Row> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,pgid=,stat=,command="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Row {
                pid: fields.next()?.parse().ok()?,
                group: fields.next()?.parse().ok()?,
                state: fields.next()?.to_owned(),
                command: fields.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

fn in_group(group: u32) -> Vec<Row> {
    table()
        .into_iter()
        .filter(|row| row.group == group)
        .collect()
}

/// Whether anything that is not a zombie is still in `group`.
fn live_in(group: u32) -> bool {
    in_group(group)
        .iter()
        .any(|row| !row.state.starts_with('Z'))
}

fn await_group_empty(group: u32, bound: Duration) -> Duration {
    let start = Instant::now();
    while in_group(group).iter().any(|row| row.pid != 0) {
        assert!(
            start.elapsed() < bound,
            "group {group} still holds {:?}",
            in_group(group)
                .iter()
                .map(|row| (row.pid, row.state.clone(), row.command.clone()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    start.elapsed()
}

fn await_gone(host: &RealHost, key: &AdoptionKey) {
    let deadline = Instant::now() + BOUND;
    while host.alive(key) != Liveness::Gone {
        assert!(Instant::now() < deadline, "the agent never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_running_agent_has_a_keeper_in_its_group_named_plainly() {
    let dir = tempfile::tempdir().expect("scratch");
    let mut host = keeping_host(Duration::from_secs(5));
    let key = launch(&mut host, "exec sleep 120", dir.path());

    let members = in_group(key.pid);
    assert!(
        members
            .iter()
            .any(|row| row.command.ends_with("duded keeper")),
        "no keeper in the agent's group: {:?}",
        members.iter().map(|row| &row.command).collect::<Vec<_>>()
    );
    assert_eq!(members.len(), 2, "the group is the agent and its keeper");

    shut_down(&mut host);
    await_group_empty(key.pid, Duration::from_secs(2));
}

#[test]
fn an_agent_that_exits_normally_leaves_no_keeper_and_no_zombie() {
    let dir = tempfile::tempdir().expect("scratch");
    let mut host = keeping_host(Duration::from_secs(5));
    let key = launch(&mut host, "sleep 1; exit 0", dir.path());
    await_gone(&host, &key);
    await_group_empty(key.pid, Duration::from_secs(1));
}

#[test]
fn an_agent_that_dies_before_its_keeper_joins_is_still_launched_and_cleaned_up() {
    let dir = tempfile::tempdir().expect("scratch");
    let mut host = keeping_host(Duration::from_secs(5));
    for _ in 0..40 {
        let key = launch(&mut host, "exit 0", dir.path());
        await_gone(&host, &key);
        await_group_empty(key.pid, Duration::from_secs(1));
    }
}

#[test]
fn an_agent_killed_by_a_signal_leaves_no_keeper_and_no_zombie() {
    let dir = tempfile::tempdir().expect("scratch");
    let mut host = keeping_host(Duration::from_secs(5));
    let key = launch(&mut host, "sleep 1; kill -9 $$", dir.path());
    await_gone(&host, &key);
    await_group_empty(key.pid, Duration::from_secs(1));
}

#[test]
fn a_stop_finishes_well_inside_its_grace_and_leaves_nothing() {
    let dir = tempfile::tempdir().expect("scratch");
    let grace = Duration::from_secs(5);
    let mut host = keeping_host(grace);
    let key = launch(&mut host, "exec sleep 120", dir.path());

    let started = Instant::now();
    let outcome = host.kill(&key);
    let stopped = started.elapsed();
    assert!(matches!(outcome, KillOutcome::Signalled), "{outcome:?}");
    assert!(
        stopped < Duration::from_secs(1),
        "the stop took {stopped:?} of a {grace:?} grace"
    );
    await_group_empty(key.pid, Duration::from_secs(1));
    assert_eq!(host.owned(), 0);
}

#[test]
fn a_keeper_that_cannot_start_fails_the_launch_and_no_task_is_written() {
    let dir = tempfile::tempdir().expect("scratch");
    let task = dir.path().join("task");
    let mut host = RealHost::with_grace(Duration::from_secs(5))
        .with_keeper(PathBuf::from("/nonexistent/duded"));
    let body = format!(
        "echo $$ > '{}'; cat > '{}'",
        dir.path().join("pid").display(),
        task.display()
    );
    let result = host.spawn(
        &script(
            &body,
            dir.path(),
            LaunchInput::WriteThenClose(b"the task".to_vec()),
        ),
        &ManagedInstall::default(),
    );
    let SpawnResult::NotLaunched { reason } = result else {
        panic!("the launch did not fail closed");
    };
    assert_eq!(reason.kind(), std::io::ErrorKind::NotFound, "{reason}");
    assert_eq!(host.owned(), 0, "a failed launch left a registered child");
    if let Ok(pid) = fs::read_to_string(dir.path().join("pid")) {
        let pid: u32 = pid.trim().parse().expect("pid");
        assert!(
            !live_in(pid),
            "the agent of a failed launch is still running"
        );
    }
    assert!(
        fs::read(&task).map_or(true, |bytes| bytes.is_empty()),
        "a task reached an agent whose keeper could not start"
    );
}

#[test]
fn starting_a_keeper_costs_little_launch_time() {
    let samples: usize = std::env::var("KEEPER_LAUNCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let dir = tempfile::tempdir().expect("scratch");
    let measure = |keeper: bool| -> Vec<Duration> {
        let mut host = if keeper {
            keeping_host(Duration::from_secs(5))
        } else {
            RealHost::with_grace(Duration::from_secs(5))
        };
        let mut times = Vec::new();
        for _ in 0..samples {
            let spec = script("exec sleep 120", dir.path(), LaunchInput::Null);
            let started = Instant::now();
            let SpawnResult::Spawned(_) = host.spawn(&spec, &ManagedInstall::default()) else {
                panic!("launch failed");
            };
            times.push(started.elapsed());
        }
        shut_down(&mut host);
        times.sort();
        times
    };
    let without = measure(false);
    let with = measure(true);
    let at = |times: &[Duration], per_cent: usize| times[(times.len() - 1) * per_cent / 100];
    #[allow(
        clippy::print_stderr,
        reason = "the measurement is the point of the test"
    )]
    {
        for (label, times) in [("without keeper", &without), ("with keeper   ", &with)] {
            eprintln!(
                "launch {label}: n={} median {:?}, p95 {:?}, worst {:?}",
                times.len(),
                at(times, 50),
                at(times, 95),
                at(times, 100)
            );
        }
    }
    assert!(
        at(&with, 100) < Duration::from_secs(1),
        "a launch with a keeper took {:?}, against a 3 s peer wait",
        at(&with, 100)
    );
}

// --- the daemon dies -------------------------------------------------------------------------

/// The helper process: it hosts agents with keepers, records them, and waits to be killed.
#[test]
fn helper_host() {
    let Some(dir) = std::env::var_os(HELPER_DIR).map(PathBuf::from) else {
        return;
    };
    let scenario = std::env::var(HELPER_SCENARIO).expect("scenario");
    let record = |key: &AdoptionKey| {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("agents"))
            .expect("agents file");
        writeln!(
            file,
            "{} {} {}",
            key.pid,
            key.start_time,
            key.boot.clone().unwrap_or_default()
        )
        .expect("record");
    };
    match scenario.as_str() {
        "one" => {
            let mut host = keeping_host(Duration::from_secs(5));
            record(&launch(&mut host, "exec sleep 300", &dir));
            fs::write(dir.join("ready"), "").expect("ready");
            std::thread::park();
        }
        "ignores_term" => {
            let mut host = keeping_host(Duration::from_secs(5));
            let body = "trap '' TERM HUP INT; touch started; while :; do sleep 1; done";
            record(&launch(&mut host, body, &dir));
            fs::write(dir.join("ready"), "").expect("ready");
            std::thread::park();
        }
        "pairs" => {
            let rounds = 15;
            let barrier = Arc::new(Barrier::new(2));
            let workers: Vec<_> = (0..2)
                .map(|worker| {
                    let dir = dir.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let mut host = keeping_host(Duration::from_secs(5));
                        let mut keys = Vec::new();
                        for round in 0..rounds {
                            let n = format!("{worker}-{round}");
                            let body = format!(
                                "cat > '{dir}/task.{n}'; touch '{dir}/done.{n}'; exec sleep 300",
                                dir = dir.display()
                            );
                            barrier.wait();
                            keys.push(launch_with(
                                &mut host,
                                &body,
                                &dir,
                                LaunchInput::WriteThenClose(b"a task".to_vec()),
                            ));
                        }
                        (host, keys)
                    })
                })
                .collect();
            let mut hosts = Vec::new();
            for worker in workers {
                let (host, keys) = worker.join().expect("worker");
                keys.iter().for_each(&record);
                hosts.push(host);
            }
            fs::write(dir.join("ready"), "").expect("ready");
            std::thread::park();
        }
        "attached" => {
            let mut ptys = dudes_pty::PtyHost::new();
            let opened = ptys
                .open(
                    script("exec sleep 300", &dir, LaunchInput::Null),
                    dudes_pty::PtySize { cols: 80, rows: 24 },
                )
                .unwrap_or_else(|_| panic!("the terminal opens"));
            let pid = ptys.pid(opened.id).expect("terminal child");
            fs::write(dir.join("agents"), format!("{pid} 0 \n")).expect("record");
            fs::write(dir.join("ready"), "").expect("ready");
            std::thread::park();
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn start_helper(dir: &Path, scenario: &str) -> Child {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "helper_host", "--nocapture", "--test-threads=1"])
        .env(HELPER_DIR, dir)
        .env(HELPER_SCENARIO, scenario)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(dir.join("helper.log")).expect("helper log"))
        .spawn()
        .expect("helper starts");
    let log = || fs::read_to_string(dir.join("helper.log")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !dir.join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "the helper never became ready: {}",
            log()
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the helper exited early: {}",
            log()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

fn recorded(dir: &Path) -> Vec<AdoptionKey> {
    fs::read_to_string(dir.join("agents"))
        .expect("agents")
        .lines()
        .map(|line| {
            let mut fields = line.split(' ');
            AdoptionKey {
                pid: fields.next().expect("pid").parse().expect("pid"),
                start_time: fields.next().expect("start").parse().expect("start"),
                boot: fields
                    .next()
                    .filter(|boot| !boot.is_empty())
                    .map(str::to_owned),
            }
        })
        .collect()
}

/// Kill the helper the way a crash does and measure how long its agents outlive it.
fn kill_daemon_and_measure(mut helper: Child, keys: &[AdoptionKey]) -> Duration {
    for key in keys {
        assert!(
            live_in(key.pid),
            "agent {} was not running before the crash",
            key.pid
        );
    }
    let killed = Instant::now();
    helper.kill().expect("SIGKILL");
    helper.wait().expect("reap helper");
    for key in keys {
        await_group_empty_live(key.pid, Duration::from_secs(5));
    }
    killed.elapsed()
}

/// Like [`await_group_empty`], but a zombie does not count: the helper is gone, so whoever adopted
/// the orphans reaps them on its own schedule.
fn await_group_empty_live(group: u32, bound: Duration) {
    let start = Instant::now();
    while live_in(group) {
        assert!(start.elapsed() < bound, "group {group} outlived its daemon");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_daemon_killed_with_sigkill_takes_its_agent_with_it_before_a_restart_probes() {
    let dir = tempfile::tempdir().expect("scratch");
    let helper = start_helper(dir.path(), "one");
    let keys = recorded(dir.path());
    let outlived = kill_daemon_and_measure(helper, &keys);
    assert!(
        outlived < Duration::from_secs(1),
        "agents outlived the daemon by {outlived:?}"
    );

    let restarted = RealHost::new();
    for key in &keys {
        assert_eq!(restarted.alive(key), Liveness::Gone);
    }
}

#[test]
fn an_agent_that_ignores_sigterm_is_ended_at_once_when_the_daemon_dies() {
    let dir = tempfile::tempdir().expect("scratch");
    let helper = start_helper(dir.path(), "ignores_term");
    let keys = recorded(dir.path());
    assert!(
        dir.path().join("started").exists() || {
            std::thread::sleep(Duration::from_millis(500));
            dir.path().join("started").exists()
        }
    );
    let outlived = kill_daemon_and_measure(helper, &keys);
    assert!(
        outlived < Duration::from_secs(1),
        "the agent outlived the daemon by {outlived:?}"
    );
}

#[test]
fn concurrent_launches_leak_no_pipe_end_into_another_child() {
    let dir = tempfile::tempdir().expect("scratch");
    let helper = start_helper(dir.path(), "pairs");
    let keys = recorded(dir.path());
    assert_eq!(keys.len(), 30);

    // A stdin write end inherited by another agent would keep its owner's task unfinished: the
    // `cat` reads to end-of-file, and end-of-file never comes while a stray copy of the write end
    // is open.
    let deadline = Instant::now() + BOUND;
    let done = || {
        fs::read_dir(dir.path())
            .expect("dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with("done."))
            })
            .count()
    };
    while done() < keys.len() {
        assert!(
            Instant::now() < deadline,
            "only {} of {} agents saw end-of-file on their stdin: a child inherited a write end",
            done(),
            keys.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // And a keeper's write end held by another child would keep that keeper from ever reading
    // end-of-file when the daemon dies.
    let outlived = kill_daemon_and_measure(helper, &keys);
    assert!(
        outlived < Duration::from_secs(2),
        "agents outlived the daemon by {outlived:?}"
    );
}

/// The attached terminal path is not given a keeper. Closing the terminal's controlling side when
/// the daemon dies makes the kernel hang up the terminal's foreground group, which ends an agent
/// that takes the default action for `SIGHUP`.
#[test]
fn an_attached_terminal_agent_is_hung_up_when_the_daemon_dies() {
    let dir = tempfile::tempdir().expect("scratch");
    let helper = start_helper(dir.path(), "attached");
    let keys = recorded(dir.path());
    let outlived = kill_daemon_and_measure(helper, &keys);
    assert!(
        outlived < Duration::from_secs(1),
        "the terminal agent outlived the daemon by {outlived:?}"
    );
}
