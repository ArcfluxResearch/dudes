//! `dudes quickstart`: a fresh process starts one daemon, completes `system.ping`, and a second
//! process reuses it.
//!
//! The seam here is the installed shape: the real `dudes` and `duded` binaries as siblings. A
//! filtered nextest run does not build the sibling daemon merely because `-p duded` is selected, so
//! the runnable clean-target check is explicit:
//!
//! ```text
//! cargo build -p duded --locked
//! cargo nextest run -p dudes --test quickstart --locked
//! ```
//!
//! A missing daemon binary is a loud failure rather than a skip.

#[cfg(unix)]
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

mod common;

use common::{Cli, Run, run_program};

const SPAWN_MARKER: &str = "quickstart started duded pid ";
const CAPTURE_DEADLINE: Duration = Duration::from_secs(45);
#[cfg(windows)]
const PARKED_STDOUT_SENTINEL: &str = "planted parked stdout";
#[cfg(windows)]
const PARKED_STDERR_SENTINEL: &str = "planted parked stderr";

fn daemon_binary() -> PathBuf {
    let mut daemon = PathBuf::from(env!("CARGO_BIN_EXE_dudes"));
    daemon.set_file_name(format!("duded{}", std::env::consts::EXE_SUFFIX));
    assert!(
        daemon.is_file(),
        "{} is absent; run `cargo build -p duded --locked` before the filtered nextest command",
        daemon.display()
    );
    daemon
}

fn base_cli(root: &Path) -> Cli {
    let mut cli = Cli::bare()
        .env("DUDES_RUNTIME_DIR", root.join("run"))
        .env("DUDES_STATE_DIR", root.join("state"));
    // What a session gives the CLI, including the names the daemon needs on each platform.
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "APPDATA",
        "SystemRoot",
        "XDG_CONFIG_HOME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cli = cli.env(name, value);
        }
    }
    cli
}

fn cli(root: &Path, daemon: &Path) -> Cli {
    base_cli(root).env("DUDES_DAEMON", daemon)
}

fn log_path(root: &Path) -> PathBuf {
    root.join("run/duded.log")
}

fn spawned_pids(root: &Path) -> Vec<u32> {
    spawned_pids_in_log(&log_path(root))
}

fn spawned_pids_in_log(log: &Path) -> Vec<u32> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.strip_prefix(SPAWN_MARKER))
        .map(|text| {
            text.parse::<u32>()
                .unwrap_or_else(|error| panic!("`{text}` is not a daemon pid: {error}"))
        })
        .collect()
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid.cast_signed()), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(error) => panic!("querying owned process {pid}: {error}"),
    }
}

#[cfg(windows)]
fn system_program(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .expect("Windows reports its system root");
    PathBuf::from(root).join("System32").join(name)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let result = run_program(
        &system_program("tasklist.exe"),
        &[
            OsString::from("/FI"),
            OsString::from(format!("PID eq {pid}")),
            OsString::from("/FO"),
            OsString::from("CSV"),
            OsString::from("/NH"),
        ],
    );
    assert_eq!(result.code(), dudes::exit::SUCCESS, "{result:?}");
    let quoted_pid = format!("\"{pid}\"");
    result
        .stdout
        .lines()
        .any(|line| line.split(',').nth(1) == Some(quoted_pid.as_str()))
}

#[cfg(windows)]
fn taskkill(pid: u32) -> Run {
    let program = system_program("taskkill.exe");
    assert!(program.is_file(), "{} is absent", program.display());
    run_program(
        &program,
        &[
            OsString::from("/PID"),
            OsString::from(pid.to_string()),
            OsString::from("/T"),
            OsString::from("/F"),
        ],
    )
}

/// Compile a no-argument process that remains alive until the test kills it.
///
/// The native test already runs inside the pinned Rust toolchain, so this avoids depending on a
/// desktop application or on whether that application delegates to another process. The source is
/// deliberately self-contained and the compiler receives typed arguments without a shell.
#[cfg(windows)]
fn parked_daemon(root: &Path) -> PathBuf {
    let source = root.join("parked-daemon.rs");
    let executable = root.join("parked-duded.exe");
    std::fs::write(
        &source,
        "use std::io::Write as _;\n\
         fn main() {\n\
             let mut stdout = std::io::stdout();\n\
             stdout.write_all(b\"planted parked stdout\\n\").unwrap();\n\
             stdout.flush().unwrap();\n\
             let mut stderr = std::io::stderr();\n\
             stderr.write_all(b\"planted parked stderr\\n\").unwrap();\n\
             stderr.flush().unwrap();\n\
             loop { std::thread::sleep(std::time::Duration::from_secs(60)); }\n\
         }\n",
    )
    .expect("write the parked daemon source");
    let rustc = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    let build = run_program(
        &rustc,
        &[
            source.into_os_string(),
            OsString::from("--edition=2024"),
            OsString::from("-o"),
            executable.as_os_str().to_owned(),
        ],
    );
    assert_eq!(
        build.code(),
        0,
        "the parked daemon did not build: {build:?}"
    );
    assert!(executable.is_file(), "{} is absent", executable.display());
    executable
}

struct DaemonGuard(Option<u32>);

impl DaemonGuard {
    fn from_root(root: &Path) -> Self {
        Self::from_log(&log_path(root))
    }

    fn from_log(log: &Path) -> Self {
        Self(spawned_pids_in_log(log).last().copied())
    }

    fn stop(&mut self) {
        let Some(pid) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        {
            let pid = Pid::from_raw(pid.cast_signed());
            let _ = kill(pid, Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(10);
            while kill(pid, None).is_ok() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if kill(pid, None).is_ok() {
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            let _ = taskkill(pid);
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn captured_with_deadline(cli: Cli, root: &Path, require_marker: bool) -> Run {
    captured_with_log_deadline(cli, &log_path(root), root, require_marker)
}

fn captured_with_log_deadline(cli: Cli, log: &Path, root: &Path, require_marker: bool) -> Run {
    captured_process_with_deadline(
        CapturedProcess::new(cli.spawn_captured(), root),
        log,
        require_marker,
        CAPTURE_DEADLINE,
    )
}

struct CapturedProcess {
    child: std::process::Child,
    daemons: DaemonLogsInTree,
    result: mpsc::Receiver<(usize, io::Result<Vec<u8>>)>,
    workers: [Option<thread::JoinHandle<()>>; 2],
    streams: [Option<Vec<u8>>; 2],
    status: Option<std::process::ExitStatus>,
    settled: bool,
}

impl CapturedProcess {
    fn new(mut child: std::process::Child, root: &Path) -> Self {
        fn reader(
            mut stream: impl io::Read + Send + 'static,
            index: usize,
            sender: mpsc::Sender<(usize, io::Result<Vec<u8>>)>,
        ) -> thread::JoinHandle<()> {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = stream.read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send((index, result));
            })
        }
        let stdout = child.stdout.take().expect("captured stdout");
        let stderr = child.stderr.take().expect("captured stderr");
        let (sender, result) = mpsc::channel();
        let workers = [
            Some(reader(stdout, 0, sender.clone())),
            Some(reader(stderr, 1, sender)),
        ];
        Self {
            child,
            daemons: DaemonLogsInTree(Some(root.to_path_buf())),
            result,
            workers,
            streams: [None, None],
            status: None,
            settled: false,
        }
    }

    fn poll(&mut self) -> io::Result<Option<Run>> {
        for worker in &mut self.workers {
            if worker.as_ref().is_some_and(thread::JoinHandle::is_finished) {
                worker
                    .take()
                    .expect("finished capture worker")
                    .join()
                    .map_err(|_| io::Error::other("capture worker panicked"))?;
            }
        }
        while let Ok((index, result)) = self.result.try_recv() {
            self.streams[index] = Some(result?);
        }
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        if self.workers.iter().all(Option::is_none) {
            let [Some(stdout), Some(stderr)] = &mut self.streams else {
                return Err(io::Error::other(
                    "capture worker returned without its output",
                ));
            };
            if let Some(status) = self.status {
                return Ok(Some(Run {
                    code: status.code(),
                    stdout: String::from_utf8_lossy(stdout).into_owned(),
                    stderr: String::from_utf8_lossy(stderr).into_owned(),
                }));
            }
        }
        Ok(None)
    }

    fn stop(&mut self) -> io::Result<()> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
            if self.status.is_none() {
                self.child.kill()?;
            }
        }
        self.daemons.stop();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = self.poll();
            if self.status.is_some() && self.workers.iter().all(Option::is_none) {
                self.release();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "capture process or its output workers did not stop",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn release(&mut self) {
        self.daemons.0 = None;
        self.settled = true;
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.stop();
        }
    }
}

fn captured_process_with_deadline(
    mut captured: CapturedProcess,
    log: &Path,
    require_marker: bool,
    duration: Duration,
) -> Run {
    let deadline = Instant::now() + duration;
    loop {
        let result = captured.poll();
        match result {
            Ok(Some(run)) => {
                if require_marker && spawned_pids_in_log(log).is_empty() {
                    captured
                        .stop()
                        .expect("cleanup after the missing spawn marker");
                    panic!("quickstart returned before writing a spawn marker: {run:?}");
                }
                captured.release();
                return run;
            }
            Ok(None) => {}
            Err(error) => {
                captured.stop().expect("cleanup after capture failure");
                panic!("capturing the CLI failed: {error}");
            }
        }
        if Instant::now() >= deadline {
            captured
                .stop()
                .expect("the CLI, daemon logs and capture workers stopped");
            panic!("inherited-handle retention: no caller EOF before the deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_human_triple(run: &Run, socket: &Path, root: &Path) {
    let daemon_log =
        std::fs::read_to_string(log_path(root)).unwrap_or_else(|error| error.to_string());
    assert_eq!(
        run.code(),
        dudes::exit::SUCCESS,
        "{run:?}\ndaemon log:\n{daemon_log}"
    );
    assert!(run.stderr.is_empty(), "{run:?}");
    assert!(
        run.stdout.contains(&format!(
            "duded {} on {}",
            env!("CARGO_PKG_VERSION"),
            socket.display()
        )),
        "the version and socket fields were not printed: {run:?}"
    );
    assert!(
        run.stdout
            .lines()
            .any(|line| line == "journal has no records yet"),
        "a fresh daemon's empty journal was not named as empty: {run:?}"
    );
}

/// A fresh process starts the daemon, reports the existing ping triple, and modes the credential
/// directory owner-only. A second process reports the same contract fields by name and the spawn
/// count stays one.
#[test]
fn quickstart_starts_one_daemon_and_a_second_run_reuses_it() {
    let root = tempfile::tempdir().expect("a temporary installation home");
    let _daemon = daemon_binary();
    let socket = dudes_rpc::local_endpoint(root.path().join("run"), dudes::env::SOCKET_NAME);

    let first = captured_with_deadline(
        base_cli(root.path()).args(["quickstart"]),
        root.path(),
        true,
    );
    let mut guard = DaemonGuard::from_root(root.path());
    assert_human_triple(&first, &socket, root.path());
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(root.path().join("run"))
            .expect("quickstart created the runtime directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the credential directory is mode {mode:04o}");
    }

    let pids = spawned_pids(root.path());
    assert_eq!(pids.len(), 1, "quickstart created {pids:?}");
    #[cfg(unix)]
    assert!(process_is_alive(pids[0]), "the started daemon is not alive");

    let second = captured_with_deadline(
        base_cli(root.path()).args(["--json", "quickstart"]),
        root.path(),
        false,
    );
    assert_eq!(second.code(), dudes::exit::SUCCESS, "{second:?}");
    assert!(second.stderr.is_empty(), "{second:?}");
    let answer: serde_json::Value = second.json();
    assert_eq!(
        answer.get("version").and_then(|value| value.as_str()),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        answer.get("socket").and_then(|value| value.as_str()),
        socket.to_str()
    );
    assert!(
        answer.get("lsn").is_some(),
        "journal position field `lsn` is absent: {answer}"
    );
    assert_eq!(
        spawned_pids(root.path()),
        pids,
        "the second quickstart created another daemon"
    );

    guard.stop();
}

/// A real caller's piped stdin closes at CLI exit while the detached daemon remains live.
#[test]
fn quickstart_does_not_pass_the_callers_piped_stdin_to_the_daemon() {
    let root = tempfile::tempdir().expect("a piped-stdin home");
    let _daemon = daemon_binary();
    let socket = dudes_rpc::local_endpoint(root.path().join("run"), dudes::env::SOCKET_NAME);
    let common::PipedRun {
        mut stdin,
        result,
        worker,
    } = base_cli(root.path())
        .args(["quickstart"])
        .spawn_with_piped_stdin();
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    let mut early = None;
    let pids = loop {
        let pids = spawned_pids(root.path());
        if !pids.is_empty() {
            break pids;
        }
        match result.try_recv() {
            Ok(run) => early = Some(run),
            Err(TryRecvError::Disconnected) => {
                worker.join().expect("report the piped worker panic");
                unreachable!("a joined sender cannot remain connected");
            }
            Err(TryRecvError::Empty) => {}
        }
        if let Some(run) = early.take() {
            worker.join().expect("the piped worker did not panic");
            panic!("quickstart returned before writing a spawn marker: {run:?}");
        }
        if Instant::now() >= deadline {
            drop(stdin);
            let mut guard = DaemonGuard::from_root(root.path());
            guard.stop();
            worker
                .join()
                .expect("the piped worker stopped after cleanup");
            panic!("inherited-handle retention: no spawn marker before the deadline");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut guard = DaemonGuard::from_root(root.path());
    let first = match early {
        Some(run) => run,
        None => match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(run) => run,
            Err(error) => {
                drop(stdin);
                guard.stop();
                worker
                    .join()
                    .expect("the piped worker stopped after cleanup");
                panic!("inherited-handle retention: no caller EOF before the deadline: {error}");
            }
        },
    };
    worker.join().expect("the piped worker did not panic");

    assert_human_triple(&first, &socket, root.path());
    assert_eq!(pids.len(), 1, "quickstart created {pids:?}");
    let write = stdin.write_all(b"must not reach duded\n");
    let flush = stdin.flush();
    let error = write
        .err()
        .or_else(|| flush.err())
        .unwrap_or_else(|| panic!("the detached daemon retained the caller's piped stdin reader"));
    assert_eq!(
        error.kind(),
        io::ErrorKind::BrokenPipe,
        "closing the caller's stdin reader produced {error:?}, raw={:?}",
        error.raw_os_error()
    );

    let second = captured_with_deadline(
        base_cli(root.path()).args(["--json", "quickstart"]),
        root.path(),
        false,
    );
    assert_eq!(second.code(), dudes::exit::SUCCESS, "{second:?}");
    assert_eq!(
        spawned_pids(root.path()),
        pids,
        "the second quickstart did not reuse the live daemon"
    );
    guard.stop();
}

/// The startup seam is not a constant-print path: the same process-level command first succeeds
/// with the real daemon, then a missing executable returns a local failure and no triple.
#[test]
fn a_failed_daemon_start_is_nonzero_and_prints_no_triple() {
    let control = tempfile::tempdir().expect("a control home");
    let daemon = daemon_binary();
    let success = cli(control.path(), &daemon).args(["quickstart"]).run();
    let mut guard = DaemonGuard::from_root(control.path());
    assert_human_triple(
        &success,
        &dudes_rpc::local_endpoint(control.path().join("run"), dudes::env::SOCKET_NAME),
        control.path(),
    );
    guard.stop();

    let failed = tempfile::tempdir().expect("a failure home");
    let missing = failed.path().join("missing-duded");
    let run = cli(failed.path(), &missing).args(["quickstart"]).run();
    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert!(
        run.stdout.is_empty(),
        "a failed start printed a triple: {run:?}"
    );
    assert!(
        run.stderr.contains(&missing.display().to_string()),
        "the failure did not name the executable: {run:?}"
    );
    assert!(spawned_pids(failed.path()).is_empty());
}

/// `DUDES_SOCK` cannot point away from the endpoint the daemon will bind. The refusal names the
/// divergence and occurs before a process is created; a guard still cleans a mutant that orphans
/// one, so the fail-side control is safe to run.
#[test]
fn a_socket_runtime_divergence_leaves_no_daemon() {
    let root = tempfile::tempdir().expect("a divergent home");
    let daemon = daemon_binary();
    let configured =
        dudes_rpc::local_endpoint(root.path().join("elsewhere"), dudes::env::SOCKET_NAME);
    let derived = dudes_rpc::local_endpoint(root.path().join("run"), dudes::env::SOCKET_NAME);
    let run = cli(root.path(), &daemon)
        .env("DUDES_SOCK", &configured)
        .args(["quickstart"])
        .run();
    let guard = DaemonGuard::from_root(root.path());
    let survivors: Vec<_> = spawned_pids(root.path())
        .into_iter()
        .filter(|pid| {
            #[cfg(unix)]
            {
                process_is_alive(*pid)
            }
            #[cfg(windows)]
            {
                let _ = pid;
                true
            }
        })
        .collect();

    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert!(
        run.stdout.is_empty(),
        "a divergence printed a triple: {run:?}"
    );
    assert!(
        survivors.is_empty(),
        "a socket/runtime divergence left daemon processes {survivors:?}"
    );
    assert!(run.stderr.contains("diverge"), "{run:?}");
    assert!(
        run.stderr.contains(&configured.display().to_string()),
        "{run:?}"
    );
    assert!(
        run.stderr.contains(&derived.display().to_string()),
        "{run:?}"
    );
    drop(guard);
}

/// A `DUDES_CONFIG` the daemon would refuse is reported on the terminal with the daemon's own
/// reason, and no daemon is started.
#[test]
fn a_refused_dudes_config_is_reported_before_a_daemon_starts() {
    for (value, reason) in [
        (
            "",
            "DUDES_CONFIG is empty; set an absolute path to the operator file or unset it",
        ),
        (
            "ops.toml",
            "DUDES_CONFIG must be an absolute path to the operator file",
        ),
    ] {
        let root = tempfile::tempdir().expect("a refused-config home");
        let daemon = daemon_binary();
        let run = cli(root.path(), &daemon)
            .env("DUDES_CONFIG", value)
            .args(["quickstart"])
            .run();
        let guard = DaemonGuard::from_root(root.path());
        assert_eq!(run.code(), dudes::exit::OTHER, "{value:?}: {run:?}");
        assert!(run.stderr.contains(reason), "{value:?}: {run:?}");
        assert!(
            spawned_pids(root.path()).is_empty(),
            "{value:?} started a daemon"
        );
        drop(guard);
    }
}

/// The spawned daemon receives the composed allowlist and none of the CLI's other ambient values.
/// `/usr/bin/env` is an absolute, argument-free, shell-free fake daemon: its log is the child
/// process's effective environment, so this is the only assertion in the package that can tell a
/// composed environment from an inherited one.
///
/// # The control, run rather than asserted
///
/// Deleting `command.env_clear();` from `src/quickstart.rs` leaves exactly the inheriting spawn
/// this test exists to refuse, and turns the exact-map assertion red naming every planted name
/// below. Restoring it turns it green. Both arms are deterministic, independent of whoever runs the suite, because `common::Cli::run`
/// `env_clear()`s the parent too, so an inheriting child leaks the planted names and nothing else.
///
/// # Why each planted name is here
///
/// `DUDES_SOCK` is set to the *derived* socket so `validate_configuration` still passes and the
/// arm reaches the spawn — proving it is consumed by the CLI rather than forwarded. `DUDES_DAEMON`
/// comes from `cli()` and is the other CLI-only seam. `DUDES_TOKEN` is an agent credential,
/// `ANTHROPIC_API_KEY` and `PLANTED_API_KEY` are vendor-credential shapes, `RUST_LOG` and
/// `NODE_OPTIONS` are the unruled-and-injection cases, `DUDES_FAULT_STALL_MARKER` is a fault
/// control, and the sentinel is a name matching no rule at all.
#[cfg(unix)]
#[test]
fn quickstart_gives_the_daemon_only_the_composed_environment() {
    let root = tempfile::tempdir().expect("an environment-control home");
    let token = "t".repeat(dudes_proto::rpc::hook::HOOK_TOKEN_LEN);
    let run = cli(root.path(), Path::new("/usr/bin/env"))
        .env("PATH", "/planted/bin")
        .env("HOME", "/planted/home")
        .env("TMPDIR", "/planted/tmp")
        .env("LANG", "planted.UTF-8")
        .env("XDG_CONFIG_HOME", "/planted/config")
        .env("APPDATA", "must-not-reach-child")
        .env("SystemRoot", "must-not-reach-child")
        .env("QUICKSTART_UNLISTED_SENTINEL", "must-not-reach-child")
        .env("DUDES_TOKEN", &token)
        .env("DUDES_SOCK", root.path().join("run").join("duded.sock"))
        .env("DUDES_FAULT_STALL_MARKER", "must-not-reach-child")
        .env("ANTHROPIC_API_KEY", "must-not-reach-child")
        .env("PLANTED_API_KEY", "must-not-reach-child")
        .env("RUST_LOG", "must-not-reach-child")
        .env("NODE_OPTIONS", "must-not-reach-child")
        .args(["quickstart"])
        .run();
    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert!(run.stdout.is_empty(), "{run:?}");

    let log =
        std::fs::read_to_string(log_path(root.path())).expect("the fake daemon's environment");
    let child_env: BTreeMap<_, _> = log
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let expected = BTreeMap::from([
        (
            "DUDES_RUNTIME_DIR".to_owned(),
            root.path().join("run").display().to_string(),
        ),
        (
            "DUDES_STATE_DIR".to_owned(),
            root.path().join("state").display().to_string(),
        ),
        ("HOME".to_owned(), "/planted/home".to_owned()),
        ("LANG".to_owned(), "planted.UTF-8".to_owned()),
        ("PATH".to_owned(), "/planted/bin".to_owned()),
        ("TMPDIR".to_owned(), "/planted/tmp".to_owned()),
        #[cfg(not(target_os = "macos"))]
        ("XDG_CONFIG_HOME".to_owned(), "/planted/config".to_owned()),
    ]);
    // Collected rather than asserted one at a time, so a failing run names *every* name that
    // crossed instead of stopping at the first. That is what makes the fail-side arm readable: an
    // inheriting spawn leaks all nine at once, and a person reproducing the mutation should see
    // all nine.
    let leaked: Vec<_> = [
        "APPDATA",
        "SystemRoot",
        #[cfg(target_os = "macos")]
        "XDG_CONFIG_HOME",
        "QUICKSTART_UNLISTED_SENTINEL",
        "DUDES_TOKEN",
        "DUDES_SOCK",
        "DUDES_DAEMON",
        "DUDES_FAULT_STALL_MARKER",
        "ANTHROPIC_API_KEY",
        "PLANTED_API_KEY",
        "RUST_LOG",
        "NODE_OPTIONS",
    ]
    .into_iter()
    .filter(|absent| child_env.contains_key(*absent))
    .collect();
    assert!(
        leaked.is_empty(),
        "these reached the daemon: {leaked:?}\nthe fake daemon log was:\n{log}"
    );
    assert_eq!(child_env, expected, "the fake daemon log was:\n{log}");
}

/// Quickstart also passes the daemon each variable the operator file's `[mcp.servers]` declares
/// and this shell holds, and nothing else beyond the allowlist. A declared variable the shell lacks
/// is named in a warning, and the daemon starts anyway. Mutants: forward only the allowlist, forward
/// an undeclared name, or refuse to start.
#[cfg(unix)]
#[test]
fn quickstart_forwards_the_declared_mcp_variables_the_shell_holds() {
    let root = tempfile::tempdir().expect("a home");
    let home = root.path().join("home");
    let config = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dudes")
    } else {
        root.path().join("xdg/dudes")
    };
    std::fs::create_dir_all(&config).expect("the operator file's directory");
    std::fs::write(
        config.join("config.toml"),
        "[mcp.servers.memory]\nurl = 'https://memory.example.com/mcp'\nbearer_env = 'MEMORY_KEY'\n\
         [mcp.servers.notes]\ncommand = 'notes-server'\nsecret_env = ['NOTES_KEY']\n",
    )
    .expect("an operator file");
    let run = cli(root.path(), Path::new("/usr/bin/env"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", root.path().join("xdg"))
        .env("MEMORY_KEY", "planted-memory-key")
        .env("UNDECLARED_KEY", "must-not-reach-child")
        .args(["quickstart"])
        .run();
    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    let log =
        std::fs::read_to_string(log_path(root.path())).expect("the fake daemon's environment");
    let child_env: BTreeMap<_, _> = log
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    assert_eq!(
        child_env.get("MEMORY_KEY").map(String::as_str),
        Some("planted-memory-key"),
        "the fake daemon log was:\n{log}"
    );
    for absent in ["NOTES_KEY", "UNDECLARED_KEY"] {
        assert!(!child_env.contains_key(absent), "{absent} crossed:\n{log}");
    }
    assert!(
        run.stderr.contains(
            "dudes: warning: MCP server `notes` names NOTES_KEY, which this shell does not set; \
             the daemon starts without it and refuses any agent that needs it"
        ),
        "{run:?}"
    );
}

/// The daemon quickstart starts reads the operator file the CLI writes.
///
/// `dudes config set` writes the file under a root only this test names: `APPDATA` on Windows,
/// `XDG_CONFIG_HOME` beside an unrelated `HOME` on Linux, and `HOME` on macOS, which crossed
/// before the config path was fixed and is the control. The daemon's startup line in its log names the file it
/// loaded, followed by ` (absent)` when there was none.
#[test]
fn a_quickstarted_daemon_reads_the_config_the_cli_wrote() {
    let root = tempfile::tempdir().expect("a config-root home");
    let daemon = daemon_binary();
    let config_root = root.path().join("config-root");
    let session = || {
        let cli = cli(root.path(), &daemon);
        if cfg!(target_os = "macos") {
            cli.env("HOME", &config_root)
        } else if cfg!(windows) {
            cli.env("APPDATA", &config_root)
        } else {
            cli.env("XDG_CONFIG_HOME", &config_root)
                .env("HOME", root.path().join("home"))
        }
    };
    let config = if cfg!(target_os = "macos") {
        config_root.join("Library/Application Support/dudes/config.toml")
    } else {
        config_root.join("dudes/config.toml")
    };

    let written = session()
        .args(["config", "set", "budget.floor_usd_per_day", "5"])
        .run();
    assert_eq!(written.code(), dudes::exit::SUCCESS, "{written:?}");
    assert!(config.is_file(), "the CLI wrote no {}", config.display());

    let started = captured_with_deadline(session().args(["quickstart"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    let log = || std::fs::read_to_string(log_path(root.path())).unwrap_or_default();
    assert_eq!(
        started.code(),
        dudes::exit::SUCCESS,
        "{started:?}\ndaemon log:\n{}",
        log()
    );
    // The accept thread can answer the ping before the daemon writes its startup line.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log().contains("; config ") {
        assert!(Instant::now() < deadline, "no startup line:\n{}", log());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        log().contains(&format!("; config {}; ", config.display())),
        "the daemon did not load {}:\n{}",
        config.display(),
        log()
    );
    guard.stop();
}

/// `DUDES_CONFIG` reaches a quickstarted daemon: the CLI writes the file it names and the daemon's
/// startup line names that file, though `HOME` points elsewhere.
#[test]
fn a_quickstarted_daemon_reads_the_file_dudes_config_names() {
    let root = tempfile::tempdir().expect("a config-override home");
    let daemon = daemon_binary();
    let config = root.path().join("elsewhere").join("ops.toml");
    std::fs::create_dir_all(config.parent().unwrap()).expect("the override's directory");
    let session = || cli(root.path(), &daemon).env("DUDES_CONFIG", &config);

    let written = session()
        .args(["config", "set", "budget.floor_usd_per_day", "5"])
        .run();
    assert_eq!(written.code(), dudes::exit::SUCCESS, "{written:?}");
    assert!(config.is_file(), "the CLI wrote no {}", config.display());

    let started = captured_with_deadline(session().args(["quickstart"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    let log = || std::fs::read_to_string(log_path(root.path())).unwrap_or_default();
    assert_eq!(
        started.code(),
        dudes::exit::SUCCESS,
        "{started:?}\ndaemon log:\n{}",
        log()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log().contains("; config ") {
        assert!(Instant::now() < deadline, "no startup line:\n{}", log());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        log().contains(&format!("; config {}; ", config.display())),
        "the daemon did not load {}:\n{}",
        config.display(),
        log()
    );
    guard.stop();
}

/// `dudes stop` stops a quickstarted daemon: the call answers, the socket then refuses, and the
/// process ends. A second `stop` finds nothing to stop.
#[cfg(unix)]
#[test]
fn stop_ends_a_quickstarted_daemon() {
    let root = tempfile::tempdir().expect("a stop home");
    let _daemon = daemon_binary();
    let started = captured_with_deadline(
        base_cli(root.path()).args(["quickstart"]),
        root.path(),
        true,
    );
    let mut guard = DaemonGuard::from_root(root.path());
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let pid = spawned_pids(root.path())[0];
    assert!(process_is_alive(pid), "the started daemon is not alive");

    let stopped = base_cli(root.path()).args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
    assert_eq!(stopped.stdout, "the daemon stopped\n");
    let ping = base_cli(root.path()).args(["ping"]).run();
    assert_eq!(
        ping.code(),
        dudes::exit::UNREACHABLE,
        "the daemon still answered after stop returned: {ping:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_alive(pid) {
        assert!(Instant::now() < deadline, "the daemon did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    guard.0 = None;

    let again = base_cli(root.path()).args(["stop", "--hard"]).run();
    assert_eq!(again.code(), dudes::exit::UNREACHABLE, "{again:?}");
}

/// `dudes start` is the way back in after an upgrade: it starts the daemon when none answers,
/// prints the ping, and asks nothing, writes nothing and hires nothing. A second run starts
/// nothing new and says the daemon is running.
#[test]
fn start_starts_one_daemon_and_a_second_run_says_it_is_running() {
    let root = tempfile::tempdir().expect("a temporary installation home");
    let _daemon = daemon_binary();
    let config = root.path().join("config.toml");
    let socket = dudes_rpc::local_endpoint(root.path().join("run"), dudes::env::SOCKET_NAME);

    let first = captured_with_deadline(
        base_cli(root.path())
            .env("DUDES_CONFIG", &config)
            .args(["start"]),
        root.path(),
        true,
    );
    let mut guard = DaemonGuard::from_root(root.path());
    assert_human_triple(&first, &socket, root.path());
    assert!(
        !first.stdout.contains("already running"),
        "a daemon that was not running was reported as running: {first:?}"
    );
    let pids = spawned_pids(root.path());
    assert_eq!(pids.len(), 1, "start created {pids:?}");

    let second = captured_with_deadline(
        base_cli(root.path())
            .env("DUDES_CONFIG", &config)
            .args(["start"]),
        root.path(),
        false,
    );
    assert_eq!(second.code(), dudes::exit::SUCCESS, "{second:?}");
    assert!(second.stderr.is_empty(), "{second:?}");
    assert!(
        second
            .stdout
            .starts_with("The daemon is already running.\n"),
        "{second:?}"
    );
    assert!(
        second.stdout.contains(&format!(
            "duded {} on {}",
            env!("CARGO_PKG_VERSION"),
            socket.display()
        )),
        "{second:?}"
    );
    assert_eq!(
        spawned_pids(root.path()),
        pids,
        "start created another daemon"
    );

    assert!(!config.exists(), "start wrote the operator file");
    let agents = base_cli(root.path())
        .env("DUDES_CONFIG", &config)
        .args(["--json", "agents"])
        .run();
    assert_eq!(agents.code(), dudes::exit::SUCCESS, "{agents:?}");
    assert_eq!(
        agents.json::<serde_json::Value>(),
        serde_json::json!([]),
        "start hired an agent"
    );

    guard.stop();
}

/// After `stop`, `start` brings the daemon back without questions.
#[cfg(unix)]
#[test]
fn start_brings_a_stopped_daemon_back() {
    let root = tempfile::tempdir().expect("a restart home");
    let _daemon = daemon_binary();
    let first = captured_with_deadline(base_cli(root.path()).args(["start"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    assert_eq!(first.code(), dudes::exit::SUCCESS, "{first:?}");

    let stopped = base_cli(root.path()).args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
    guard.0 = None;

    let again = captured_with_deadline(base_cli(root.path()).args(["start"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    assert_eq!(again.code(), dudes::exit::SUCCESS, "{again:?}");
    assert_eq!(spawned_pids(root.path()).len(), 2);
    guard.stop();
}

/// A running daemon of another version is left alone and named: the user chooses when to restart.
#[cfg(unix)]
#[test]
fn start_names_a_running_daemon_of_another_version() {
    let home = tempfile::tempdir().expect("home");
    let socket = home.path().join("duded.sock");
    common::ping_as(&socket, "0.0.1");
    let cli = Cli::new(&socket, home.path()).env("HOME", home.path());

    let run = cli.clone().args(["start"]).run();
    assert_eq!(run.code(), dudes::exit::SUCCESS, "{run:?}");
    assert_eq!(
        run.stdout,
        format!(
            "The daemon is already running.\nduded 0.0.1 on {}\njournal has no records yet\n\
             The running daemon is 0.0.1 and this dudes is {}. Run dudes stop, then dudes start, \
             to use the new version.\n",
            socket.display(),
            env!("CARGO_PKG_VERSION")
        )
    );

    let machine = cli.args(["--json", "start"]).run();
    assert_eq!(machine.code(), dudes::exit::SUCCESS, "{machine:?}");
    assert_eq!(
        machine.json::<serde_json::Value>()["version"],
        "0.0.1",
        "{machine:?}"
    );
}

/// The first run in an installed layout, with the Floor bundle beside `duded`: `quickstart`, then
/// `floor open` prints a working sign-in link with no Floor variable set anywhere, and
/// `stop` ends it. On Windows the daemon binds the Floor listener only with the `SystemRoot` that
/// the CLI forwards.
#[test]
fn quickstart_serves_the_floor_installed_beside_duded() {
    const FLOOR_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../duded/tests/fixtures/floor-release-v3"
    );
    let root = tempfile::tempdir().expect("an installed layout");
    let install = root.path().join("bin");
    std::fs::create_dir_all(install.join("floor/assets")).expect("the install directory");
    let daemon = install.join(format!("duded{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(daemon_binary(), &daemon).expect("copy duded");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        std::fs::copy(
            Path::new(FLOOR_FIXTURE).join(path),
            install.join("floor").join(path),
        )
        .expect("copy the Floor bundle");
    }
    let home = root.path().join("home");
    std::fs::create_dir(&home).expect("a home");
    let installed = || cli(root.path(), &daemon).env("HOME", &home);

    let started = captured_with_deadline(installed().args(["quickstart"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    let log = || std::fs::read_to_string(log_path(root.path())).unwrap_or_default();
    assert_eq!(
        started.code(),
        dudes::exit::SUCCESS,
        "{started:?}\nlog:\n{}",
        log()
    );

    let opened = installed().args(["floor", "open"]).run();
    assert_eq!(
        opened.code(),
        dudes::exit::SUCCESS,
        "{opened:?}\nlog:\n{}",
        log()
    );
    let link = opened.stdout.lines().next().unwrap_or_default();
    assert!(
        link.starts_with("http://127.0.0.1:") && link.contains("/floor/#bootstrap="),
        "the first line is the sign-in link: {opened:?}"
    );
    let origin = link
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .expect("the link's host and port");
    let mut page = std::net::TcpStream::connect(origin).expect("the Floor listener");
    page.write_all(format!("GET /floor/ HTTP/1.1\r\nHost: {origin}\r\n\r\n").as_bytes())
        .expect("ask for the page");
    let mut response = String::new();
    io::Read::read_to_string(&mut page, &mut response).expect("read the page");
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "the Floor page: {response}"
    );
    assert!(
        log().contains(&format!("; Floor at http://{origin}/floor/")),
        "the daemon's log does not announce the Floor:\n{}",
        log()
    );

    let stopped = installed().args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
    guard.stop();
}

/// A quickstarted daemon's managed child sees the user name of the daemon's uid.
///
/// Quickstart forwards no `USER` (`bins/dudes/src/env.rs`), and the Claude CLI finds its keychain
/// login by that name, so without it every turn failed "Not logged in". The caller's own `USER` is
/// a decoy that must not cross. The planted `claude` records what its environment says, and
/// `id -un` asks the user database the same question independently of the daemon.
#[cfg(unix)]
#[test]
fn a_quickstarted_daemons_managed_child_sees_its_uids_user_name() {
    let root = tempfile::tempdir().expect("a first-run layout");
    let _daemon = daemon_binary();
    let bin = root.path().join("bin");
    let home = root.path().join("home");
    let agent_tree = root.path().join("agent");
    let seen = root.path().join("seen-user");
    #[cfg(target_os = "macos")]
    let config = home.join("Library/Application Support/dudes/config.toml");
    #[cfg(not(target_os = "macos"))]
    let config = home.join(".config/dudes/config.toml");
    for dir in [
        bin.as_path(),
        agent_tree.as_path(),
        config.parent().expect("config parent"),
    ] {
        std::fs::create_dir_all(dir).expect("a first-run directory");
    }
    std::fs::write(&config, "[budget]\nper_turn_floor = { usd = 0.000001 }\n")
        .expect("write the reserve configuration");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s' \"${{USER-unset}}\" > '{seen}.tmp' && mv '{seen}.tmp' '{seen}'\n",
            seen = seen.display()
        ),
    )
    .expect("write the planted claude");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700))
        .expect("make the planted claude executable");
    let path = std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
        .expect("a search path");
    let first_run = || {
        base_cli(root.path())
            .env("USER", "quickstart-forwards-no-user")
            .env("HOME", &home)
            .env("PATH", &path)
    };

    let started = captured_with_deadline(first_run().args(["quickstart"]), root.path(), true);
    let mut guard = DaemonGuard::from_root(root.path());
    assert_eq!(started.code(), dudes::exit::SUCCESS, "{started:?}");
    let hired = first_run()
        .arg("--json")
        .args(common::hire_args(&agent_tree))
        .run();
    assert_eq!(hired.code(), dudes::exit::SUCCESS, "{hired:?}");
    let agent = hired
        .json::<dudes_proto::rpc::agent::HireResult>()
        .agent
        .to_string();
    let goal = first_run()
        .args(common::goal_args(&agent, "reply with the single word ok"))
        .run();
    assert_eq!(goal.code(), dudes::exit::SUCCESS, "{goal:?}");

    let deadline = Instant::now() + Duration::from_secs(20);
    while !seen.exists() {
        assert!(
            Instant::now() < deadline,
            "the managed child did not run within 20s; daemon log:\n{}",
            std::fs::read_to_string(log_path(root.path())).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let id = run_program(Path::new("/usr/bin/id"), &[OsString::from("-un")]);
    assert_eq!(id.code(), 0, "{id:?}");
    assert_eq!(
        std::fs::read_to_string(&seen).expect("the child's USER"),
        id.stdout.trim_end(),
        "the managed child's USER is not the user name of the daemon's uid"
    );

    // The native host cannot prove that nothing the child started left its process group, so a
    // stop after any managed turn warns. It still succeeds, so `stop && quickstart` restarts.
    let stopped = first_run().args(["stop", "--hard"]).run();
    assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
    assert_eq!(stopped.stdout, "the daemon stopped\n");
    assert_eq!(
        stopped.stderr,
        "dudes: warning: the daemon stopped and every agent process it started has exited, but it \
         cannot prove that nothing those processes started left their process group or job\n"
    );
    let after = first_run().args(["ping"]).run();
    assert_eq!(after.code(), dudes::exit::UNREACHABLE, "{after:?}");
    guard.0 = None;
    let restarted = captured_with_deadline(first_run().args(["quickstart"]), root.path(), false);
    let mut guard = DaemonGuard::from_root(root.path());
    assert_eq!(restarted.code(), dudes::exit::SUCCESS, "{restarted:?}");
    guard.stop();
}

/// A Windows startup timeout kills and waits for the process quickstart spawned.
///
/// The test compiles an absolute, no-argument process that stays parked and cannot answer
/// `system.ping`, so the production deadline reaches the cleanup branch. `taskkill` is both the
/// process-table probe and the mutation cleanup: it exits nonzero after the candidate's
/// `Child::kill` + `wait`, but succeeds and removes the survivor when that one cleanup call is
/// deleted.
#[cfg(windows)]
#[test]
fn a_timed_out_windows_quickstart_kills_the_spawned_daemon() {
    let root = tempfile::tempdir().expect("a timeout home");
    let daemon = parked_daemon(root.path());

    let run = cli(root.path(), &daemon).args(["quickstart"]).run();
    let pids = spawned_pids(root.path());
    assert_eq!(pids.len(), 1, "quickstart created {pids:?}: {run:?}");
    let survivor = taskkill(pids[0]);

    assert_eq!(run.code(), dudes::exit::OTHER, "{run:?}");
    assert!(run.stdout.is_empty(), "a timeout printed a triple: {run:?}");
    assert!(
        run.stderr.contains("within 30 seconds"),
        "the run did not reach the timeout branch: {run:?}"
    );
    assert_ne!(
        survivor.code(),
        dudes::exit::SUCCESS,
        "the timed-out quickstart left pid {} alive; taskkill removed it: {survivor:?}",
        pids[0]
    );
    let log = std::fs::read_to_string(log_path(root.path())).expect("the parked daemon log");
    assert!(
        log.contains(PARKED_STDOUT_SENTINEL),
        "the detached daemon's stdout handle did not reach its log:\n{log}"
    );
    assert!(
        log.contains(PARKED_STDERR_SENTINEL),
        "the detached daemon's stderr handle did not reach its log:\n{log}"
    );
}

struct DaemonLogsInTree(Option<PathBuf>);

impl DaemonLogsInTree {
    fn stop(&mut self) {
        fn stop_in_tree(path: &Path) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stop_in_tree(&entry.path());
                } else if entry.file_name() == "duded.log" {
                    DaemonGuard::from_log(&entry.path()).stop();
                }
            }
        }
        if let Some(root) = &self.0 {
            stop_in_tree(root);
        }
    }
}

impl Drop for DaemonLogsInTree {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
#[test]
fn windows_quickstart_without_home_uses_profile_storage_and_reuses_the_daemon() {
    for _ in 0..5 {
        let root = tempfile::tempdir().expect("isolated profile quickstart");
        let mut owned = DaemonLogsInTree(Some(root.path().to_path_buf()));
        let profile = root.path().join("profile with spaces");
        let runtime = profile.join(".local/state/dudes");
        let state = profile.join(".local/share/dudes");
        let log = runtime.join("duded.log");
        let endpoint = dudes_rpc::local_endpoint(&runtime, "duded.sock");
        let session = base_cli(root.path())
            .without("HOME")
            .without("DUDES_RUNTIME_DIR")
            .without("DUDES_STATE_DIR")
            .env("USERPROFILE", &profile)
            .env("APPDATA", root.path().join("config"));
        let begun = Instant::now();
        let first = captured_with_log_deadline(
            session.clone().args(["--json", "quickstart"]),
            &log,
            root.path(),
            true,
        );
        let mut guard = DaemonGuard::from_log(&log);
        assert_eq!(first.code(), dudes::exit::SUCCESS, "{first:?}");
        assert!(begun.elapsed() < Duration::from_secs(30));
        let first_ping: dudes_proto::rpc::system::PingResult = first.json();
        assert_eq!(first_ping.socket, endpoint);
        assert!(
            state.join("LOCK").is_file(),
            "no lock at {}",
            state.display()
        );
        assert!(
            state.join("journal").is_dir(),
            "no journal at {}",
            state.display()
        );
        assert!(!root.path().join("run").exists());
        assert!(!root.path().join("state").exists());
        let pids = spawned_pids_in_log(&log);
        assert_eq!(pids.len(), 1);
        assert!(process_is_alive(pids[0]), "the started daemon is not alive");
        let second = captured_with_log_deadline(
            session.clone().args(["--json", "quickstart"]),
            &log,
            root.path(),
            false,
        );
        assert_eq!(second.code(), dudes::exit::SUCCESS, "{second:?}");
        let second_ping: dudes_proto::rpc::system::PingResult = second.json();
        assert_eq!(second_ping.socket, endpoint);
        assert_eq!(spawned_pids_in_log(&log), pids);
        let stopped = session.clone().args(["stop"]).run();
        assert_eq!(stopped.code(), dudes::exit::SUCCESS, "{stopped:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_is_alive(pids[0]) {
            assert!(Instant::now() < deadline, "the daemon survived stop");
            thread::sleep(Duration::from_millis(20));
        }
        guard.0 = None;
        let after = session.args(["ping"]).run();
        assert_ne!(
            after.code(),
            dudes::exit::SUCCESS,
            "daemon answers after stop"
        );
        owned.0 = None;
    }
}

fn retained_handle_fixture(root: &Path) -> PathBuf {
    let source = root.join("retained-handles.rs");
    let executable = root.join(format!("retained-handles{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(
        &source,
        r#"use std::{env, fs, path::PathBuf, process::{Command, Stdio}, thread, time::Duration};
fn main() {
    let argument = env::args_os().nth(1).unwrap();
    if argument == "--daemon" {
        thread::sleep(Duration::from_secs(120));
        return;
    }
    let log = PathBuf::from(argument);
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let child = Command::new(env::current_exe().unwrap())
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn().unwrap();
    fs::write(log, format!("quickstart started duded pid {}\n", child.id())).unwrap();
    thread::sleep(Duration::from_secs(120));
}
"#,
    )
    .expect("write the retained-handle fixture");
    let rustc = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    let built = run_program(
        &rustc,
        &[
            source.into_os_string(),
            "--edition=2024".into(),
            "-o".into(),
            executable.as_os_str().to_owned(),
        ],
    );
    assert_eq!(built.code(), 0, "{built:?}");
    executable
}

#[test]
fn capture_timeout_reaps_a_retained_handle_daemon_at_an_alternate_root() {
    let root = tempfile::tempdir().expect("alternate-root capture fixture");
    let _owned = DaemonLogsInTree(Some(root.path().to_path_buf()));
    let actual_log = root.path().join("alternate/duded.log");
    let expected_log = root.path().join("expected/duded.log");
    let executable = retained_handle_fixture(root.path());
    let child = common::process(
        executable.as_os_str(),
        &[actual_log.as_os_str().to_owned()],
        None,
    )
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("start the retained-handle fixture");
    let cli_pid = child.id();
    let mut captured = CapturedProcess::new(child, root.path());
    let fixture_deadline = Instant::now() + Duration::from_secs(30);
    while spawned_pids_in_log(&actual_log).is_empty() {
        if let Some(status) = captured.child.try_wait().expect("fixture startup status") {
            captured.status = Some(status);
            if !status.success() {
                captured.stop().expect("cleanup after fixture exit");
                panic!("the retained-handle fixture exited with {status}");
            }
        }
        if Instant::now() >= fixture_deadline {
            captured
                .stop()
                .expect("cleanup after fixture startup failure");
            panic!("the retained-handle fixture wrote no spawn marker");
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        process_is_alive(spawned_pids_in_log(&actual_log)[0]),
        "the handle-retaining daemon is not alive"
    );
    let capture_wait = Duration::from_millis(200);
    let capture_deadline = Instant::now() + capture_wait;
    let timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        captured_process_with_deadline(captured, &expected_log, true, capture_wait)
    }));
    let cleanup_elapsed = capture_deadline.elapsed();
    let cli_survived = process_is_alive(cli_pid);
    let pids = spawned_pids_in_log(&actual_log);
    let daemon_survived = pids.first().is_some_and(|pid| process_is_alive(*pid));
    if cli_survived {
        DaemonGuard(Some(cli_pid)).stop();
    }
    if let Some(pid) = pids.first()
        && daemon_survived
    {
        DaemonGuard(Some(*pid)).stop();
    }
    assert_eq!(pids.len(), 1);
    assert!(!daemon_survived, "the alternate-root daemon survived");
    assert!(!cli_survived, "the capture process survived");
    assert!(
        cleanup_elapsed < Duration::from_secs(9),
        "alternate-root cleanup exceeded nine seconds after the capture deadline: {cleanup_elapsed:?}"
    );
    let timeout = timeout.expect_err("the missing expected log did not time out");
    assert_eq!(
        timeout.downcast_ref::<&str>(),
        Some(&"inherited-handle retention: no caller EOF before the deadline"),
        "capture failed before its inherited-handle deadline"
    );
}
