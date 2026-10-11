//! A live daemon for the CLI to talk to, and the way this suite runs the real binary against it.
//!
//! # The one thing that makes these tests worth anything
//!
//! **The test daemon runs the shipped `dudes_rpc::PeerClassifier`** — the one `duded::boot` builds
//! for itself, over the credential it mints and the token projection its own loop publishes. Not a
//! hand-rolled authorizer or classifier: this fixture supplies one
//! input to it, the peer-classification column, and nothing else. That is the difference between *"the classifier
//! refused"* and *"nothing is registered"*, and it is why the refusing arm of `doors.rs` is
//! evidence rather than a coincidence:
//!
//! - Presenting nothing resolves to `Unproven`.
//! - Presenting the credential this daemon minted lands in the `NoRecordedAgent`
//!   column of `classify` and resolves to `Operator`.
//! - The `-32001` on the four operator commands then comes from each method's **`Requirement`**, at its
//!   registration, checked once on the dispatch path.
//!
//! **Same daemon, same registrations, only the credential's readability differs.** A refusal that
//! came from an unregistered method could not survive that pairing, because the second half is
//! admitted through the very same registrations.
//!
//! # What is fake, and what that costs
//!
//! - **The peer resolver is a fake** answering `NoRecordedAgent` for every peer, handed in through
//!   `Seams.resolver`. The daemon's own resolver answers `Unresolvable`, which is the other half
//!   of the same peer-classification column and decides identically — so what this fixture buys is not a
//!   different verdict but a named column, and these tests say nothing about the first, second or
//!   fourth. This fake does not walk process ancestry; the daemon's own resolver does.
//! - **The process host is a fake**, so a goal starts a turn without needing a provider CLI on the
//!   box.
//! - **The token projection is not fake.** It is the daemon's own, published by its own loop.
//!   The operator-door fixtures present none; `token.rs` gives `dudes ask` a separately minted
//!   token and Agent column, because that is the binary's one agent-callable method.
//!
//! # What no test in this package can show
//!
//! **A `duded` started as a separate process.** The daemon below runs on a thread of this test,
//! not out of `env!("CARGO_BIN_EXE_duded")`, so what is asserted is the daemon's composition
//! rather than the binary's argv and environment handling. The classifier is not part of that
//! narrowing — this *is* the shipped classifier — and `bins/duded/tests/wire.rs` carries the same
//! statement from the daemon's own side.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::settings::{SOCKET_NAME, Seams, Settings};
use duded::startup::Witness;
use dudes_journal::{Fsync, JournalOptions, Reader};
use dudes_policy::BreakerConfig;
use dudes_proto::{Entry, Record, Spend};
use dudes_providers::adapter::{Adapter, ManagedInstall};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::registry::Adapters;
use dudes_rpc::auth::{PeerIdentity, PeerProvenance, PeerResolution, PeerResolver};
use dudes_supervisor::host::{
    AdoptionKey, Clock, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};
use dudes_supervisor::testing::{FakeHost, FakeRng};
use dudes_types::{Lsn, Timestamp, Tokens, Usd};

// ---------------------------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------------------------

/// A `Sync` clock the daemon can share, whose `now()` advances one millisecond per read.
///
/// `dudes_supervisor::testing::FakeClock` is now also `Sync` and could satisfy this seam. This
/// package-local clock remains to avoid folding an unrelated CLI-fixture refactor;
/// it carries no distinct rule and returns deterministic timestamps starting from the value
/// configured here.
#[derive(Debug)]
pub(crate) struct TestClock {
    at: AtomicI64,
}

impl TestClock {
    pub(crate) const fn new() -> Self {
        Self {
            at: AtomicI64::new(1_787_394_600_000),
        }
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.at.fetch_add(1, Ordering::SeqCst))
    }
}

/// `FakeHost` behind a lock, so a test keeps a handle on it after `run` has taken the daemon.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharedHost(
    Arc<Mutex<FakeHost>>,
    Option<ShutdownHold>,
    Option<fn() -> io::Error>,
);

impl SharedHost {
    pub(crate) fn new() -> Self {
        Self(
            Arc::new(Mutex::new(FakeHost::wall_clock_anchored())),
            None,
            None,
        )
    }

    /// A host whose `shutdown` waits at `hold`, so a test can act while the daemon is mid-exit.
    pub(crate) fn holding_shutdown(hold: ShutdownHold) -> Self {
        Self(Self::new().0, Some(hold), None)
    }

    pub(crate) fn failing_shutdown() -> Self {
        Self(
            Self::new().0,
            None,
            Some(|| io::Error::other("injected unresolved managed child ownership")),
        )
    }

    /// A host whose every retained child exited and was reaped, as the native host reports after
    /// any managed turn.
    pub(crate) fn unproven_containment_shutdown() -> Self {
        Self(
            Self::new().0,
            None,
            Some(|| io::Error::other(dudes_supervisor::host::ContainmentUnproven)),
        )
    }

    fn peek<T>(&self, f: impl FnOnce(&FakeHost) -> T) -> T {
        f(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// How many children this host has been asked to create.
    pub(crate) fn children(&self) -> usize {
        self.peek(|host| host.spawned_pids.len())
    }
}

impl ProcessHost for SharedHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if let Some(hold) = &self.1 {
            hold.enter();
        }
        if let Some(failure) = self.2 {
            return Err(failure());
        }
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown()
    }
}

/// Where a [`SharedHost::holding_shutdown`] host's sweep waits: entered by the daemon's exit,
/// released by the test. The wait is bounded, so a failed test still lets its daemon finish.
#[derive(Clone, Debug, Default)]
pub(crate) struct ShutdownHold(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl ShutdownHold {
    fn enter(&self) {
        let (state, changed) = &*self.0;
        let mut held = state.lock().unwrap_or_else(PoisonError::into_inner);
        held.0 = true;
        changed.notify_all();
        let _ = changed.wait_timeout_while(held, Duration::from_secs(30), |held| !held.1);
    }

    /// Whether the daemon reached its host sweep within `bound`.
    pub(crate) fn wait_entered(&self, bound: Duration) -> bool {
        let (state, changed) = &*self.0;
        let held = state.lock().unwrap_or_else(PoisonError::into_inner);
        changed
            .wait_timeout_while(held, bound, |held| !held.0)
            .is_ok_and(|(held, _)| held.0)
    }

    pub(crate) fn release(&self) {
        let (state, changed) = &*self.0;
        state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        changed.notify_all();
    }
}

/// A predicate runner that creates no process and answers "not yet".
///
/// Every goal in this suite stays open, which is what the loop tests need: a predicate that
/// passed immediately would make `Done` race the assertion that a turn was issued at all. A test
/// that wants a check to pass calls [`OpenRunner::pass`], and every later run passes.
#[derive(Debug, Default)]
pub(crate) struct OpenRunner {
    runs: AtomicUsize,
    passing: AtomicBool,
}

impl OpenRunner {
    /// From now on every run of a check passes.
    pub(crate) fn pass(&self) {
        self.passing.store(true, Ordering::SeqCst);
    }
}

impl PredicateRunner for OpenRunner {
    fn run(&self, _spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let passing = self.passing.load(Ordering::SeqCst);
        Ok(PredicateOutcome::Finished {
            exit: Some(i32::from(!passing)),
            output_tail: "not yet".to_owned(),
            timed_out: false,
        })
    }

    fn sweep(&self) {}
}

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

/// A daemon's two directories, held so dropping the fixture removes both trees.
pub(crate) struct Fixture {
    pub(crate) state: tempfile::TempDir,
    pub(crate) runtime: tempfile::TempDir,
    /// A directory that never holds a credential, for the arm that presents nothing.
    pub(crate) empty: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self {
            state: tempfile::tempdir().expect("a state directory"),
            runtime: tempfile::tempdir().expect("a runtime directory"),
            empty: tempfile::tempdir().expect("an empty directory"),
        }
    }

    /// Settings over this fixture's directories.
    ///
    /// `Fsync::Full`, like the daemon's own tests: invariant 2 is what the loop tests rest on, and
    /// a journal configured not to sync would make "persist before spawn" untestable by definition.
    pub(crate) fn settings(&self) -> Settings {
        Settings {
            state_dir: self.state.path().to_path_buf(),
            // `run/` inside the temporary directory, because `bind_in` creates the directory itself
            // and refuses one that already exists with the wrong mode.
            runtime_dir: self.runtime.path().join("run"),
            socket_name: SOCKET_NAME.to_owned(),
            journal: JournalOptions {
                fsync: Fsync::Full,
                ..JournalOptions::default()
            },
            floor_usd_per_day: Some(Usd::from_dollars(100)),
            per_turn_floor: Spend {
                usd: Usd::from_micros(1),
                tokens: Tokens::new(1),
                work_tokens: Tokens::new(1),
            },
            enrollment: duded::floor_actions::Enrollment::default(),
            floor_answer_enabled: false,
            floor_hire_enabled: false,
            floor_control: duded::settings::FloorControl::Off,
            floor_terminal_enabled: false,
            floor_git_enabled: false,
            floor_board_enabled: false,
            // Off by default here too: nothing in this suite exercises the
            // memory-condense model path, and turning it on would let the daemon's own resolution
            // step find this harness's stand-in `claude` and hand it to a real `PredicateRunner`.
            condense_model_enabled: false,
            condense_model: None,
            operator_config: None,
            breaker: BreakerConfig::default(),
            // A stand-in `claude` first on the managed PATH, because the daemon resolves the
            // provider's program before the launch reaches the fake host, which never runs it.
            daemon_env: vec![EnvVar::new("PATH", {
                let bin = self.runtime.path().join("bin");
                std::fs::create_dir_all(&bin).expect("a stand-in bin directory");
                let cli = if cfg!(windows) {
                    "claude.exe"
                } else {
                    "claude"
                };
                std::fs::write(bin.join(cli), b"").expect("a stand-in managed CLI");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(bin.join(cli), std::fs::Permissions::from_mode(0o755))
                        .expect("a runnable stand-in");
                }
                std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                    .expect("a joinable PATH")
            })],
            predicate_env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
            predicate_path: Some(OsString::from("/usr/bin:/bin")),
            git_checkout_deadline: duded::worktree::git::CHECKOUT_DEADLINE,
            max_connections: NonZeroUsize::new(8).expect("not zero"),
            // Far longer than any test here: a beat that fired mid-assertion would make a test about
            // a door a test about timing.
            beat: Duration::from_secs(3600),
            floor: None,
            hook_command: Some({
                let path = self.runtime.path().join("dudes-hook");
                std::fs::write(&path, b"").expect("a stand-in hook command");
                path
            }),
            daemon_exe: None,
        }
    }

    /// Every entry on the disk right now, in lsn order.
    ///
    /// Panics on a read failure rather than returning an empty vector: assertions here count
    /// records, and a journal that could not be read would satisfy a count of zero.
    pub(crate) fn entries(&self) -> Vec<(Lsn, Entry)> {
        let dir = self.state.path().join(duded::boot::JOURNAL_DIR);
        if !dir.exists() {
            return Vec::new();
        }
        Reader::new(&dir)
            .iter_from(Lsn::ZERO)
            .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
            .map(|item| item.unwrap_or_else(|error| panic!("reading {}: {error}", dir.display())))
            .collect()
    }

    /// Every record on the disk right now.
    pub(crate) fn records(&self) -> Vec<Record> {
        self.entries()
            .into_iter()
            .map(|(_, entry)| entry.record)
            .collect()
    }

    /// Block until `f` finds what it is looking for in the journal, or fail loudly.
    pub(crate) fn await_record<T>(&self, what: &str, f: impl Fn(&Record) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = self.records().iter().find_map(&f) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no {what} reached the journal within 10s; records: {:?}",
                self.records().iter().map(Record::kind).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`PeerResolver`] answering one column for every peer.
///
/// The daemon's own answers `Unresolvable`; this names `NoRecordedAgent`, the other half of the
/// same peer-classification column, so the tests below can cite a column by name.
#[derive(Debug)]
pub(crate) struct ScriptedColumn(PeerProvenance);

impl PeerResolver for ScriptedColumn {
    fn resolve(&self, _peer: &PeerIdentity) -> PeerResolution {
        PeerResolution::Resolved(self.0)
    }
}

/// The seams a test daemon runs on. The classifier is not among them: `duded::boot` builds the
/// shipped one, and what this chooses is its column.
pub(crate) fn seams(
    host: SharedHost,
    runner: Arc<OpenRunner>,
) -> Seams<SharedHost, OpenRunner, TestClock, FakeRng> {
    seams_with(
        host,
        runner,
        Arc::new(ScriptedColumn(PeerProvenance::NoRecordedAgent)),
    )
}

/// [`seams`] with a chosen column, for the arms that vary it.
pub(crate) fn seams_with(
    host: SharedHost,
    runner: Arc<OpenRunner>,
    resolver: Arc<dyn PeerResolver>,
) -> Seams<SharedHost, OpenRunner, TestClock, FakeRng> {
    Seams {
        host,
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: runner,
        clock: Arc::new(TestClock::new()),
        rng: FakeRng::new(),
        resolver: duded::classify::Resolver::fixed(resolver),
        // The registry the shipped daemon builds: the default Claude adapter. A test that needs
        // a provider *absent* asks for it here.
        adapters: Adapters::new([
            Box::new(ClaudeAdapter::new(ClaudeConfig::default())) as Box<dyn Adapter>
        ]),
        // A fake host opens no pipe, so nothing ever reaches this sink here; it is supplied
        // because the daemon's executor registers every spawned child against one. The test that
        // drives a real child through it is `bins/duded/tests/managed_turn.rs`.
        children: duded::ChildEnds::new(),
        witness: Witness::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// The running daemon
// ---------------------------------------------------------------------------------------------

/// A daemon serving on a thread of its own.
pub(crate) struct Running {
    pub(crate) append_failure: duded::runtime::AppendFailure,
    pub(crate) handle: duded::Handle,
    // `()`, not the daemon's `anyhow::Result<ExitCause>`: naming that type would put `anyhow` in
    // this package's dev-dependencies to hold a value nothing here reads. A daemon that failed to
    // run surfaces immediately and unmistakably anyway — every CLI run against it exits
    // `UNREACHABLE`, with the socket path in the message.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    pub(crate) fn stop(&mut self) {
        self.handle.stop(duded::ExitCause::Ended);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Running {
    /// A test that failed an assertion still takes its daemon down, so no accept loop outlives it.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Boot a daemon on this fixture and start it on a thread.
pub(crate) fn start(settings: &Settings, host: SharedHost, runner: Arc<OpenRunner>) -> Running {
    start_with(settings, seams(host, runner))
}

/// Boot and start with chosen seams.
pub(crate) fn start_with(
    settings: &Settings,
    seams: Seams<SharedHost, OpenRunner, TestClock, FakeRng>,
) -> Running {
    let daemon = duded::boot(settings, seams).expect("boot the test daemon");
    let handle = daemon.handle();
    let append_failure = daemon.append_failure();
    let thread = std::thread::Builder::new()
        .name("duded-under-test".to_owned())
        .spawn(move || {
            let _ = daemon.run();
        })
        .expect("the daemon thread");
    Running {
        append_failure,
        handle,
        thread: Some(thread),
    }
}

// ---------------------------------------------------------------------------------------------
// Running the real binary
// ---------------------------------------------------------------------------------------------

/// What running `dudes` produced.
#[derive(Debug)]
pub(crate) struct Run {
    /// The process's exit code. `None` when it was killed by a signal, which is a failure here.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Run {
    /// The exit code, insisting there was one and that it is a byte.
    ///
    /// Returned as `u8` so it compares directly against `dudes::exit`'s constants: a test that had
    /// to widen one of those at every site is a test where the wrong constant reads the same as the
    /// right one.
    pub(crate) fn code(&self) -> u8 {
        let code = self.code.unwrap_or_else(|| {
            panic!(
                "the CLI was killed by a signal\nstdout: {}\nstderr: {}",
                self.stdout, self.stderr
            )
        });
        u8::try_from(code).unwrap_or_else(|_| panic!("exit code {code} is not a byte"))
    }

    /// Everything the process wrote, on either stream.
    pub(crate) fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    /// The first line of stdout, parsed as `T` — what `--json` promises.
    pub(crate) fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        let line = self.stdout.lines().next().unwrap_or_else(|| {
            panic!("the CLI printed nothing on stdout\nstderr: {}", self.stderr)
        });
        serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("`{line}` is not this method's result type: {error}"))
    }

    /// The error object `--json` wrote on stderr.
    pub(crate) fn json_error(&self) -> dudes_proto::rpc::RpcError {
        let line = self.stderr.lines().next().unwrap_or_else(|| {
            panic!("the CLI printed nothing on stderr\nstdout: {}", self.stdout)
        });
        serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("`{line}` is not an RpcError: {error}"))
    }
}

/// One invocation of the real `dudes` binary, with an environment named explicitly.
///
/// # The environment is built, never inherited
///
/// `env_clear`, then exactly what the test names. Inheriting would make every assertion here depend
/// on the developer's own shell — a `DUDES_SOCK` in the environment would point the CLI at a real
/// daemon, and a `DUDES_TOKEN` would silently satisfy the test that asserts one is discovered.
/// `Clone` so a test can keep one configured `Cli` and spend it several times — [`Cli::run`]
/// consumes it, which is what stops a half-built invocation from being reused by accident.
#[derive(Clone)]
pub(crate) struct Cli {
    env: BTreeMap<OsString, OsString>,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
}

impl Cli {
    /// A CLI pointed at `socket`, reading its credential from `runtime_dir`.
    pub(crate) fn new(socket: &Path, runtime_dir: &Path) -> Self {
        let mut env = BTreeMap::new();
        env.insert(OsString::from("DUDES_SOCK"), socket.as_os_str().to_owned());
        env.insert(
            OsString::from("DUDES_RUNTIME_DIR"),
            runtime_dir.as_os_str().to_owned(),
        );
        Self {
            env,
            args: Vec::new(),
            cwd: None,
        }
    }

    /// A CLI with no environment at all, for the arms about a missing socket.
    pub(crate) fn bare() -> Self {
        Self {
            env: BTreeMap::new(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// Set an environment variable for this run.
    pub(crate) fn env(mut self, name: &str, value: impl AsRef<OsStr>) -> Self {
        self.env
            .insert(OsString::from(name), value.as_ref().to_owned());
        self
    }

    /// Remove one.
    pub(crate) fn without(mut self, name: &str) -> Self {
        self.env.remove(OsStr::new(name));
        self
    }

    /// Append arguments.
    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    /// Append one argument, whose bytes are whatever they are.
    pub(crate) fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Run in `dir` rather than in the test's own working directory.
    pub(crate) fn current_dir(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    /// Run it through this package's one test-side process seam.
    pub(crate) fn run(self) -> Run {
        let mut command = process(
            OsStr::new(env!("CARGO_BIN_EXE_dudes")),
            &self.args,
            Some(&self.env),
        );
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        let output = command.output().expect("run the dudes binary");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    pub(crate) fn spawn_captured(self) -> std::process::Child {
        let mut command = process(
            OsStr::new(env!("CARGO_BIN_EXE_dudes")),
            &self.args,
            Some(&self.env),
        );
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the captured dudes binary")
    }

    /// Spawn the real CLI with all three standard streams on `terminal`, a pseudo-terminal's
    /// follower side, as an operator at a terminal runs it.
    #[cfg(unix)]
    pub(crate) fn spawn_on_terminal(self, terminal: &std::os::fd::OwnedFd) -> std::process::Child {
        let mut command = process(
            OsStr::new(env!("CARGO_BIN_EXE_dudes")),
            &self.args,
            Some(&self.env),
        );
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        let stream = || Stdio::from(terminal.try_clone().expect("share the terminal"));
        command
            .stdin(stream())
            .stdout(stream())
            .stderr(stream())
            .spawn()
            .expect("spawn the dudes binary on a terminal")
    }

    /// Spawn the real CLI with all three standard streams piped, retaining the caller's stdin
    /// writer while a worker drains both output streams and waits for exit.
    pub(crate) fn spawn_with_piped_stdin(self) -> PipedRun {
        let mut command = process(
            OsStr::new(env!("CARGO_BIN_EXE_dudes")),
            &self.args,
            Some(&self.env),
        );
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn the dudes binary");
        let stdin = child.stdin.take().expect("the caller's piped stdin writer");
        let (sender, result) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let output = child
                .wait_with_output()
                .expect("read the dudes binary's output and wait for exit");
            let _ = sender.send(Run {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        });
        PipedRun {
            stdin,
            result,
            worker,
        }
    }
}

/// A captured CLI process whose caller retains the standard-input writer.
pub(crate) struct PipedRun {
    pub(crate) stdin: ChildStdin,
    pub(crate) result: Receiver<Run>,
    pub(crate) worker: JoinHandle<()>,
}

/// Run a platform program with typed arguments and the test process's environment.
///
/// Windows process-lifetime tests use this for the pinned Rust compiler and an absolute
/// `taskkill.exe` path, and the quickstart user-name test for `/usr/bin/id`. It is deliberately the
/// same process seam as [`Cli::run`]: the package keeps one clippy exemption, and neither caller
/// synthesizes a shell string.
pub(crate) fn run_program(program: &Path, args: &[OsString]) -> Run {
    let output = process(program.as_os_str(), args, None)
        .output()
        .expect("run the platform program");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The one `std::process::Command` in this package's tests.
///
/// `bins/dudes/clippy.toml` disallows the type outright, and this is the single site that answers it
/// with an attribute and a reason. An exit code is a property of a process: the CLI tests need the
/// real binary, and the Windows lifetime test needs the platform's real process-table command.
///
/// A `Some` environment is exact and cleared first; `None` inherits for a platform program whose
/// own runtime may need ambient system variables. The program and every argument are separate
/// `OsStr` values, never a shell command.
#[expect(
    clippy::disallowed_types,
    reason = "an exit code is a property of a process; the source scan covers src/ regardless"
)]
pub(crate) fn process(
    program: &OsStr,
    args: &[OsString],
    env: Option<&BTreeMap<OsString, OsString>>,
) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    if let Some(env) = env {
        command.env_clear();
        for (name, value) in env {
            command.env(name, value);
        }
    }
    command.args(args);
    command
}

// ---------------------------------------------------------------------------------------------
// Argument lines the suite reuses
// ---------------------------------------------------------------------------------------------

/// A well-formed `hire` line for a Claude-driven agent in `cwd`.
pub(crate) fn hire_args(cwd: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "hire",
        "--name",
        "Hector",
        "--role",
        "infra",
        "--department",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(OsString::from("infra"));
    args.push(OsString::from("--provider"));
    args.push(OsString::from("claude"));
    args.push(OsString::from("--cwd"));
    args.push(cwd.as_os_str().to_owned());
    args.push(OsString::from("--usd-per-day"));
    args.push(OsString::from("10"));
    args
}

/// A well-formed `goal` line for `agent`, whose goal text is `text`. Its check, `/usr/bin/true`,
/// is declared as one that may already pass, so the daemon does not run it first.
pub(crate) fn goal_args(agent: &str, text: &str) -> Vec<OsString> {
    [
        "goal",
        "--agent",
        agent,
        "--text",
        text,
        "--usd",
        "5",
        "--hop-cap",
        "8",
        "--check-may-already-pass",
        "--",
        "/usr/bin/true",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// A stand-in daemon on `socket` that answers every `system.ping` as daemon `version`, and any
/// other method with an error, for as long as the test process runs.
#[cfg(unix)]
pub(crate) fn ping_as(socket: &Path, version: &str) {
    use std::io::{BufRead as _, Write as _};
    let listener = std::os::unix::net::UnixListener::bind(socket).expect("a stand-in daemon");
    let ping = serde_json::json!({"version": version, "socket": socket, "lsn": 0});
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap_or(0)
                == 0
            {
                continue;
            }
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let reply = if request["method"] == "system.ping" {
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": ping})
            } else {
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32601, "message": "this stand-in answers only system.ping"}})
            };
            let _ = writeln!(&stream, "{reply}");
        }
    });
}
