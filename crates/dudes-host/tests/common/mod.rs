//! The helper program every process test in this crate launches, and the machinery for watching it.
//!
//! # The helper is this test binary
//!
//! There is no separate helper executable. Each test binary re-executes **itself** with
//! `DUDES_HOST_TEST_ROLE` set, and [`run_if_helper`] — called from a one-line `#[test]` in each
//! file that needs it — notices the variable and becomes the helper instead of returning. A normal
//! run does not set the variable, so that test passes instantly and costs nothing.
//!
//! The alternative was a `[[bin]]`, and it was rejected for a reason worth writing down: a bin
//! target in this crate would be published with the crate and picked up by the release packaging,
//! so a test fixture would ship to users. `std::env::current_exe` costs nothing and ships nothing.
//!
//! # Why most helpers report through files rather than through a pipe
//!
//! A [`LaunchSpec`] describes argv, environment, working directory and stdin, and deliberately says
//! nothing about stdout. **That is not because there is no pipe** — a host built with
//! `RealHost::watching` pipes every agent child's standard output and reads it, because a managed
//! turn's result is parsed out of it — but because the pipe is not a per-launch choice: it is a
//! property of the host, so there is nothing for a spec to say (`src/stream.rs`).
//!
//! What follows for these helpers is that a *test* only sees the stream if it built a watching
//! host and supplied a sink, which `tests/stream.rs` does and the rest do not. Every other helper
//! therefore writes what its test needs into its working directory: its own identifier, and a
//! counter it keeps bumping. Every write goes to a temporary name and is then renamed over the
//! target, so a reader never sees half a file.
//!
//! # The heartbeat, and why "still alive" needs one
//!
//! Two of the tests turn on a process being alive *at a particular moment*, and no probe can settle
//! that: a child that has exited but not yet been reaped is still in the process table with its
//! original start value, so it answers `Alive` to the very check the host uses. A counter the child
//! keeps advancing distinguishes running from dead-but-unreaped, which a probe cannot, and it is
//! the child itself doing the reporting rather than the code under test.
//!
//! # A `LEAK` verdict against a test in this crate does not name a leaked child
//!
//! **This section is here because it is where a reader who has just seen one arrives.** The
//! verdict is real, it is reproducible, and the test it names is not the guilty one — usually
//! because there is no guilty one.
//!
//! Every helper reached through [`helper_spec`] is started by a `RealHost` **with no sink**, which
//! leaves the child's standard output and standard error at the standard library's default — the
//! parent's — for the reason `src/lib.rs` gives where it says so. (`tests/stream.rs` is the one
//! exception: it builds a watching host, so its child's stdout is a pipe this process reads to the
//! end and its stderr is still inherited.) Under `cargo nextest` the parent's are the runner's
//! capture pipe, so a helper outliving its test *would* hold that pipe open and be reported. **No
//! helper does.** Every test here stops every child it spawned before it returns: `RealHost::kill`
//! reaps a successfully killed child, while `RealHost::shutdown` waits only when direct exit is
//! independently bounded and may retain the row after the process stops; and the two roles that survive a
//! signal — [`GRANDCHILD`]'s grandchild, and whatever [`EXEC`] and [`DEAF`] replace themselves
//! with — are started by `helper_command`, whose three descriptors are the null device.
//!
//! nextest reports `LEAK` on roughly one run in forty of this suite anyway, against whichever test
//! it was accounting at the time, tests that spawn nothing included. Measured rather than argued:
//!
//! - the suite as it stands, nextest's default threads: **4 leaky runs of 150**;
//! - identical, but with *every* child's stdio forced to the null device, so that no descendant of
//!   any test can hold a pipe at all: **3 of 150** — the rate does not move;
//! - identical, `--test-threads 64`: **9 of 60**, and six of the nine name a test that spawns no
//!   process, four of them `kill::tests::*`, which are scripted against a fake target;
//! - `--test-threads 1` beside a second nextest running this same suite in parallel — the same
//!   machine contention, but never two of its own tests at once: **0 of 30**, while the parallel
//!   one leaked twice in 92;
//! - 150 runs sampled every 20 ms for a surviving `the_helper_entry_point` process: **none**.
//!
//! The second line settles what is being measured: with no possible holder the rate is unchanged,
//! so the verdict is not about a held pipe. The fourth says the ingredient is concurrency inside
//! one runner rather than a busy machine. So the verdict is a threshold on the runner's own
//! bookkeeping, `--test-threads 64` is the control that makes it fail on demand, and
//! `.config/nextest.toml` carries the threshold that clears that control and the reasoning for its
//! value.
//!
//! **The one case where a `LEAK` here would be real is a different defect, and it is not this
//! one.** A test that panics before the `shutdown()` or `kill()` that would stop its children never
//! reaps them: `std::process::Child`'s `Drop` neither signals nor waits. What limits the damage is
//! accidental rather than designed — the helpers write their heartbeat into the test's temporary
//! directory, `TempDir`'s own `Drop` removes it as the panic unwinds, and each helper then panics
//! on its next write, a [`BEAT`] later. Forced: with the anti-vacuity window in
//! [`beating`] shrunk to zero so that `tests/shutdown.rs`'s assertion cannot pass, the panic lands
//! there as expected, no helper survives the run, and **no `LEAK` is reported** even at nextest's
//! shipped 100 ms. So a `LEAK` seen beside that failure is downstream of the panic and not the
//! same defect as the one this section is about, which fires with every assertion passing.
//!
//! SIGKILL skips the temporary directory's destructor, so every helper also exits after
//! [`HELPER_LIFETIME`]. This bound is longer than the tests and does not replace their kill assertions.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use dudes_host::RealHost;
use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::host::{AdoptionKey, ProcessHost, SpawnResult};

/// Which role the re-executed binary should play.
const ROLE: &str = "DUDES_HOST_TEST_ROLE";
/// Where the re-executed binary should write its identifier.
const MARK: &str = "DUDES_HOST_TEST_MARK";

/// Sits still and keeps a heartbeat.
pub(crate) const PARK: &str = "park";
/// Spawns a `park` grandchild **without** putting it in a group of its own, so the grandchild
/// inherits the helper's group — which is what makes it reachable only by a group sweep.
pub(crate) const GRANDCHILD: &str = "grandchild";
/// Writes its mark, waits for a `go` file beside it, then replaces its own image with `park` in
/// place — keeping its identifier and the start value the kernel recorded when it was created.
pub(crate) const EXEC: &str = "exec";
/// Writes its mark and exits at once, closing the standard input the host handed it. A host
/// delivering more than a pipe buffer of `stdin` then fails **after** the process has been
/// created, which is the only way a test can reach that arm of `spawn`.
pub(crate) const BRIEF: &str = "brief";
/// Writes its mark and immediately replaces its own image with `park`, whose standard input is the
/// null device — so the pipe the host handed it is closed while the process stays alive and keeps
/// beating. Unix only: the redirect is done by the exec, and there is no exec in place on Windows.
pub(crate) const DEAF: &str = "deaf";
/// Writes one `stream-json` turn to its **standard output** and exits 0: a `system/init` line that
/// announces a session, then [`FLOOD`] bytes of `assistant` filler, then a `result` line carrying a
/// cost. The filler is the whole point of the role — it is what fills a pipe nobody drains, so a
/// host that piped without reading blocks this child for ever instead of finishing it.
pub(crate) const STREAM: &str = "stream";
/// Writes more than a pipe of output before reading standard input, then reports whether the exact
/// input payload reached it and whether its bounded deadlock release had to fire.
pub(crate) const DUPLEX: &str = "duplex";

/// How many bytes of filler the [`STREAM`] role writes before its `result` line.
const FLOOD: &str = "DUDES_HOST_TEST_FLOOD";

/// How long the [`DUPLEX`] helper waits for its early output to drain before breaking an old
/// host's two-full-pipes cycle itself.
const DUPLEX_RELEASE: Duration = Duration::from_secs(10);

/// The session id the [`STREAM`] role announces.
pub(crate) const STREAM_SESSION: &str = "3f9a1c2e-0000-4000-8000-00000000dead";

/// More than any pipe buffer on any platform this runs on, so a helper that closes or abandons its
/// standard input makes the host's write fail rather than quietly buffering all of it.
pub(crate) const MORE_THAN_A_PIPE_HOLDS: usize = 1 << 20;

/// How long the helper waits between heartbeats.
const BEAT: Duration = Duration::from_millis(20);

/// Leave room for the ten-second duplex release while bounding a killed runner's orphan helpers.
const HELPER_LIFETIME: Duration = Duration::from_secs(60);

/// How long a freshly spawned helper gets to write its first beats before a control gives up on it.
const STARTUP: Duration = Duration::from_secs(2);

/// The environment variables a child needs to be a working process at all. The host hands a child
/// exactly the environment the spec names, so anything not listed here is genuinely absent — which
/// is the point, but a Windows process with no `SystemRoot` does not start.
const PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "SystemRoot",
    "windir",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "SystemDrive",
];

/// Become the helper if this process was launched as one; return immediately otherwise.
///
/// Never returns in the helper case: each role exits or replaces its own image.
pub(crate) fn run_if_helper() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let mark = PathBuf::from(std::env::var(MARK).expect("a helper is always given a mark path"));
    let _watchdog = std::thread::spawn(|| {
        sleep(HELPER_LIFETIME);
        std::process::exit(0);
    });
    match role.as_str() {
        PARK => {
            write_atomic(&mark, &std::process::id().to_string());
            park(&mark);
        }
        GRANDCHILD => {
            let grandchild = mark.with_file_name("grandchild.pid");
            // No `process_group`: the grandchild stays in the helper's group on purpose. A signal
            // aimed at the helper's identifier alone never reaches it, so only a group sweep can.
            let mut command = helper_command(PARK, &grandchild);
            // Never waited on, deliberately: this helper parks forever, and the grandchild's whole
            // purpose is to be a descendant that only a group sweep can reach. Reaping it here
            // would remove the thing the test is about.
            #[allow(clippy::zombie_processes)]
            let _grandchild = command
                .spawn()
                .expect("the helper could not spawn a grandchild");
            write_atomic(&mark, &std::process::id().to_string());
            park(&mark);
        }
        EXEC => {
            write_atomic(&mark, &std::process::id().to_string());
            // Wait to be told, so the test can read the key *before* the exec and compare it with
            // the same key read after. Without the handshake the exec might already have happened
            // by the time the first read lands, and the comparison would prove nothing.
            let go = mark.with_file_name("go");
            while !go.exists() {
                sleep(BEAT);
            }
            let after = mark.with_file_name("after.pid");
            exec_in_place(&after);
        }
        BRIEF => {
            write_atomic(&mark, &std::process::id().to_string());
            // Not `return`: the host is mid-write into this process's standard input, and the
            // point of this role is that the read end goes away while it is.
            std::process::exit(0);
        }
        DEAF => {
            write_atomic(&mark, &std::process::id().to_string());
            // The exec is what closes the inherited pipe: `helper_command` sets the child's
            // standard input to the null device, and an exec in place applies that redirect to
            // this very process. Closing fd 0 directly would need `unsafe`, which this crate
            // grants to exactly one module and no test.
            exec_in_place(&mark);
        }
        STREAM => {
            // Before the stream, so a test that only wants the descriptor need not wait for the
            // turn — and so the report is on disk even if the write below blocks on a full pipe.
            //
            // Gated rather than twinned: `stdout_identity` is Unix-only for the reason its own
            // doc comment records, and the file written here is read only by
            // `await_stdout_identity`, whose callers all sit inside the one `#[cfg(unix)]` test
            // in `stream.rs` — so nothing on Windows reads it. A `#[cfg(windows)]` twin in
            // `exec_in_place`'s shape would have to panic, and unlike that role's panic this one
            // would be reachable: the other stream tests drive this same arm ungated, and
            // everything else it does is portable.
            #[cfg(unix)]
            write_atomic(&stdout_id_path(&mark), &stdout_identity());
            write_atomic(&mark, &std::process::id().to_string());
            write_stream_turn();
            std::process::exit(0);
        }
        DUPLEX => {
            write_atomic(&mark, &std::process::id().to_string());
            write_duplex_turn();
            std::process::exit(0);
        }
        other => panic!("unknown helper role `{other}`"),
    }
}

/// Write a pipe-filling prefix before reading standard input, with a bounded release for the
/// deliberately broken parent implementation.
fn write_duplex_turn() {
    use std::io::{Read as _, Write as _};

    let (written_tx, written_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let filler = "d".repeat(4096);
        let mut written = 0usize;
        while written <= MORE_THAN_A_PIPE_HOLDS {
            writeln!(
                out,
                r#"{{"type":"assistant","message":{{"content":"{filler}"}}}}"#
            )
            .expect("the duplex helper writes its early output");
            written += filler.len();
        }
        out.flush()
            .expect("the duplex helper flushes its early output");
        let _ = written_tx.send(());
    });

    let release_fired = written_rx.recv_timeout(DUPLEX_RELEASE).is_err();
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .expect("the duplex helper reads standard input");
    writer.join().expect("the duplex stdout writer finishes");
    let stdin_matches = input.iter().all(|&byte| byte == b'i');

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        r#"{{"type":"result","stdin_bytes":{},"stdin_matches":{stdin_matches},"release_fired":{release_fired}}}"#,
        input.len()
    )
    .expect("the duplex helper writes its result");
    out.flush().expect("the duplex helper flushes its result");
}

/// One `stream-json` turn on standard output, filler and all.
///
/// `writeln!` on the handle rather than `println!`, because `clippy::print_stdout` is denied across
/// this crate and this is a helper *being* a vendor CLI rather than a crate printing.
fn write_stream_turn() {
    use std::io::Write as _;

    let flood: usize = std::env::var(FLOOD)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{STREAM_SESSION}","tools":[]}}"#
    );
    // One filler line at a time, so the child is doing what a real vendor CLI does: writing a
    // stream of lines while the host is expected to be reading them.
    let filler = "z".repeat(4096);
    let mut written = 0usize;
    while written < flood {
        let _ = writeln!(
            out,
            r#"{{"type":"assistant","session_id":"{STREAM_SESSION}","message":{{"content":"{filler}"}}}}"#
        );
        written += filler.len();
    }
    let _ = writeln!(
        out,
        r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{STREAM_SESSION}","result":"the task is done","total_cost_usd":0.5,"modelUsage":{{"claude-opus-4":{{"inputTokens":7,"outputTokens":11,"cacheReadInputTokens":13,"cacheCreationInputTokens":17}}}}}}"#
    );
    let _ = out.flush();
}

/// Keep a counter moving so a watcher can tell running from dead-but-unreaped.
fn park(mark: &Path) -> ! {
    let beat = beat_path(mark);
    let mut count: u64 = 0;
    loop {
        count += 1;
        write_atomic(&beat, &count.to_string());
        sleep(BEAT);
    }
}

/// Replace this process's image with the `park` role, keeping the identifier and — the thing under
/// test — the start value the kernel recorded when the process was first created.
#[cfg(unix)]
fn exec_in_place(after: &Path) -> ! {
    use std::os::unix::process::CommandExt as _;
    let error = helper_command(PARK, after).exec();
    panic!("the helper could not exec in place: {error}");
}

/// Windows has no exec-in-place, so the role is never asked for there.
#[cfg(windows)]
fn exec_in_place(_after: &Path) -> ! {
    panic!("exec in place is a Unix role and this platform has no equivalent");
}

/// This binary, re-invoked at the one `#[test]` that calls [`run_if_helper`].
fn helper_command(role: &str, mark: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current_exe"));
    for arg in harness_argv() {
        command.arg(arg);
    }
    command.env_clear();
    for (name, value) in passthrough() {
        command.env(name, value);
    }
    command.env(ROLE, role);
    command.env(MARK, mark);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command
}

/// The arguments that make the test harness run exactly the helper entry point and nothing else.
fn harness_argv() -> Vec<OsString> {
    ["--exact", "the_helper_entry_point", "--quiet"]
        .iter()
        .map(OsString::from)
        .collect()
}

/// The passthrough variables that are actually set in this process.
fn passthrough() -> Vec<(OsString, OsString)> {
    PASSTHROUGH
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(*name), value)))
        .collect()
}

/// A [`LaunchSpec`] that launches this binary as `role`, reporting into `mark`.
///
/// The task text a real spec carries is one argv element and never a shell string; the same holds
/// here — every element below is a typed part and nothing is concatenated.
pub(crate) fn helper_spec(role: &str, mark: &Path, cwd: &Path) -> LaunchSpec {
    let mut argv = vec![OsString::from(
        std::env::current_exe().expect("current_exe"),
    )];
    argv.extend(harness_argv());
    let mut env: Vec<EnvVar> = passthrough()
        .into_iter()
        .map(|(name, value)| EnvVar::new(name, value))
        .collect();
    env.push(EnvVar::new(ROLE, role));
    env.push(EnvVar::new(MARK, mark.as_os_str()));
    LaunchSpec {
        argv,
        env,
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    }
}

/// Which open file description this process's standard output is, as `"<device>:<inode>"`.
///
/// The point is *identity*, not shape. Asking "is my stdout a pipe?" cannot tell an inherited
/// descriptor from a fresh one under `cargo nextest`, whose own stdout is already a pipe; asking
/// *which* description it is can, because an inherited stdout is literally the same one the parent
/// holds and a pipe the host created is not.
///
/// Unix only: `fstat` through `rustix`, which this crate already takes as a dev-dependency with
/// `fs` for the umask test. Windows has an equivalent (`GetFileInformationByHandle`) and no safe
/// wrapper in this graph, so the one test that uses this is gated rather than approximated.
#[cfg(unix)]
pub(crate) fn stdout_identity() -> String {
    let stat = rustix::fs::fstat(std::io::stdout()).expect("this process has a standard output");
    format!("{}:{}", stat.st_dev, stat.st_ino)
}

/// Where the [`STREAM`] role reports [`stdout_identity`] for the helper reporting at `mark`.
#[cfg(unix)]
pub(crate) fn stdout_id_path(mark: &Path) -> PathBuf {
    mark.with_extension("stdout-id")
}

/// Block until the helper reporting at `mark` has said which standard output it was given.
///
/// Panics rather than returning an error, for [`await_number`]'s reason: a helper that never
/// reports is a broken test and not a case to handle.
#[cfg(unix)]
pub(crate) fn await_stdout_identity(mark: &Path) -> String {
    let path = stdout_id_path(mark);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = fs::read_to_string(&path)
            && !text.trim().is_empty()
        {
            return text.trim().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "the helper never reported its standard output at {}",
            path.display()
        );
        sleep(Duration::from_millis(10));
    }
}

/// [`helper_spec`] for the [`STREAM`] role, with `flood` bytes of filler before the `result` line.
pub(crate) fn stream_spec(mark: &Path, cwd: &Path, flood: usize) -> LaunchSpec {
    let mut spec = helper_spec(STREAM, mark, cwd);
    spec.env.push(EnvVar::new(FLOOD, flood.to_string()));
    spec
}

/// A [`LaunchSpec`] for the [`DUPLEX`] role with exactly `stdin` delivered through its pipe.
pub(crate) fn duplex_spec(mark: &Path, cwd: &Path, stdin: Vec<u8>) -> LaunchSpec {
    let mut spec = helper_spec(DUPLEX, mark, cwd);
    spec.stdin = dudes_providers::launch::LaunchInput::WriteThenClose(stdin);
    spec
}

/// A managed install with no files: the adapter projection is not what these tests are about.
pub(crate) fn no_install() -> ManagedInstall {
    ManagedInstall::default()
}

/// Require the success arm in tests whose subject starts after launch.
pub(crate) fn spawned(result: SpawnResult, context: &str) -> AdoptionKey {
    match result {
        SpawnResult::Spawned(key) => key,
        SpawnResult::NotLaunched { reason } | SpawnResult::CleanupUnconfirmed { reason } => {
            panic!("{context}: {reason}")
        }
    }
}

/// Incidental teardown for tests whose subject is elsewhere. Production grips do not prove
/// complete arbitrary-tree coverage, so shutdown stops what it can address, retains the row and
/// reports either its first concrete error or the synthetic unresolved-ownership error.
pub(crate) fn shutdown_retaining_incomplete(host: &mut RealHost, context: &str) -> io::Error {
    let Err(error) = host.shutdown() else {
        panic!("{context}: nonempty production host discarded incomplete ownership");
    };
    assert_ne!(
        host.owned(),
        0,
        "{context}: error returned without retention"
    );
    assert!(
        !error.to_string().is_empty(),
        "{context}: empty shutdown error"
    );
    error
}

/// Where the heartbeat for the helper that reports at `mark` lives.
pub(crate) fn beat_path(mark: &Path) -> PathBuf {
    mark.with_extension("beat")
}

/// Block until `path` holds a parsable number, and return it. Panics rather than returning an
/// error: a helper that never reports is a broken test, not a case to handle.
pub(crate) fn await_number(path: &Path) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = read_number(path) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "nothing readable appeared at {} within 20s",
            path.display()
        );
        sleep(Duration::from_millis(10));
    }
}

/// The number `path` holds right now, if it holds one.
pub(crate) fn read_number(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether the process reporting at `beat` is still running *now*.
///
/// Two readings across several heartbeats: a process that is running advances the counter, and one
/// that has stopped — exited, terminated, or exited and not yet reaped — does not. The window is
/// long enough for several beats so that one slow scheduler slice cannot read as death.
pub(crate) fn beating(beat: &Path) -> bool {
    beating_within(beat, BEAT * 15)
}

/// Whether a process that was just spawned is running: waits up to `STARTUP` for its first beats,
/// which a loaded machine can delay well past the short window [`beating`] watches. Use it for the
/// "it is running before I act" control only; a check that something stopped keeps [`beating`], so
/// it still fails promptly when the process is still advancing.
pub(crate) fn beating_after_start(beat: &Path) -> bool {
    beating_within(beat, STARTUP)
}

/// Whether the process reporting at `beat` advances within `window`.
///
/// Pre-property controls use a wider window than post-kill checks: scheduling may delay a live
/// helper, while a dead helper can never advance regardless of how long it is watched.
pub(crate) fn beating_within(beat: &Path, window: Duration) -> bool {
    let before = read_number(beat);
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if read_number(beat) > before {
            return true;
        }
        sleep(BEAT / 2);
    }
    false
}

/// Write `text` where a concurrent reader can only ever see all of it or none of it.
///
/// The temporary name carries both the target's name and the writer's identifier, so two helpers
/// sharing a directory — a child and the grandchild it spawned — cannot collide on it.
fn write_atomic(path: &Path, text: &str) {
    let name = path
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned();
    let temporary = path.with_file_name(format!(".{name}.{}.writing", std::process::id()));
    fs::write(&temporary, text).expect("write");
    fs::rename(&temporary, path).expect("rename");
}

// --- reading this crate's own source, for the scans that pin a text-level invariant ------------

/// This crate's `src/` directory, found from the manifest rather than from the current directory,
/// so the scans work under any runner.
pub(crate) fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, at any depth, so a module added later is scanned too.
pub(crate) fn rust_sources() -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect(&src_dir(), &mut found);
    found.sort();
    found
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("src/ is where it was") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `text` that are code rather than prose.
///
/// A line counts as prose when the first thing on it is `//` — which covers `//!` module docs,
/// `///` item docs and every `// SAFETY:` comment. That distinction is not decoration: the modules
/// in this crate have to be able to *discuss* `unsafe`, `btime` and `NoSuchPid` at length in order
/// to explain why they do not use them, and a scan that matched prose would either forbid the
/// explanation or be switched off.
///
/// Deliberately simple, and its one blind spot is worth naming: a trailing comment on a code line
/// is still scanned. That errs toward flagging rather than toward missing, which is the direction
/// a guard should err in.
pub(crate) fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect()
}

/// The tokens from `tokens` that appear in `text`'s code lines. The single point at which every
/// scan in this crate is applied, so a scan and the self-test that proves it works cannot drift.
pub(crate) fn found_in_code<'t>(text: &str, tokens: &[&'t str]) -> Vec<&'t str> {
    let code = code_lines(text);
    tokens
        .iter()
        .copied()
        .filter(|token| code.iter().any(|line| line.contains(token)))
        .collect()
}

/// How many times `token` appears in `text`'s code lines.
///
/// The counting counterpart of [`found_in_code`], and it shares [`code_lines`] with it for the same
/// reason: a scan that read prose would count the sentences explaining a grant as part of the
/// grant. Where `found_in_code` answers "is this shape present at all", this answers "how much of
/// it is there" — which is what turns a guard on *where* `unsafe` lives into a guard on *how much*
/// of it there is.
pub(crate) fn count_in_code(text: &str, token: &str) -> usize {
    code_lines(text)
        .iter()
        .map(|line| line.matches(token).count())
        .sum()
}

/// Tell an `exec` helper reporting at `mark` that it may replace its image now.
pub(crate) fn release_exec(mark: &Path) {
    fs::write(mark.with_file_name("go"), b"go").expect("write the go file");
}
