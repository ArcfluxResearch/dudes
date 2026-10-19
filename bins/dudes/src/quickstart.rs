//! Start the daemon on first use, then return the existing `system.ping` result.
//!
//! This module creates exactly one process: an absolute `duded` path with no arguments. It never
//! inherits a terminal or the caller's environment, and readiness is a successful call rather
//! than the return of `spawn` or the presence of a socket file.

#[cfg(unix)]
use std::fs::DirBuilder;
use std::fs::OpenOptions;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use dudes_proto::rpc::system::PingResult;
#[cfg(windows)]
use windows_spawn::Child;

use crate::env::{Environment, NoConfig, NoDirectory, SOCKET_NAME};

const START_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const LOG_NAME: &str = "duded.log";
const SPAWN_MARKER: &str = "quickstart started duded pid ";

/// A local failure before `system.ping` completed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// A directory the daemon needs is not named.
    #[error(transparent)]
    NoDirectory(#[from] NoDirectory),
    /// `DUDES_CONFIG` is set to a value the daemon refuses.
    #[error(transparent)]
    RefusedConfig(NoConfig),
    /// The CLI endpoint is not where this daemon configuration will bind.
    #[error(
        "DUDES_SOCK and DUDES_RUNTIME_DIR diverge: the configured endpoint is {configured}, but the daemon will bind {derived}",
        configured = .configured.display(),
        derived = .derived.display()
    )]
    ConfiguredSocketDiverges {
        configured: PathBuf,
        derived: PathBuf,
    },
    /// The socket path cannot be bound on this platform.
    #[error(transparent)]
    SocketPath(#[from] SocketPathTooLong),
    /// A responding peer reported a different endpoint.
    #[error(
        "system.ping reported socket {reported}, which diverges from the endpoint {endpoint}",
        reported = .reported.display(),
        endpoint = .endpoint.display()
    )]
    ReportedSocketDiverges {
        reported: PathBuf,
        endpoint: PathBuf,
    },
    /// The executable cannot be resolved without a `PATH` search.
    #[error("resolving the `duded` sibling of the running `dudes`: {0}")]
    ResolveDaemon(#[source] io::Error),
    /// `DUDES_DAEMON` named a relative path, which would be resolved through the working directory.
    #[error(
        "the daemon path must be absolute (set DUDES_DAEMON to an absolute path): {path}",
        path = .0.display()
    )]
    RelativeDaemon(PathBuf),
    /// Creating the runtime directory failed.
    #[error(
        "creating the runtime directory {path}: {source}",
        path = .path.display()
    )]
    RuntimeDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Opening the daemon's detached log failed.
    #[error("opening the daemon log {path}: {source}", path = .path.display())]
    Log {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The one spawn call failed.
    #[error("starting the daemon at {path}: {source}", path = .path.display())]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The detached child exited before it served the call.
    #[error("the daemon exited with {0} before system.ping answered")]
    Exited(ExitStatus),
    /// Inspecting the child failed.
    #[error("checking whether the daemon exited before system.ping answered: {0}")]
    Wait(#[source] io::Error),
    /// No answer arrived by the fixed readiness deadline.
    #[error("the daemon did not answer system.ping within 30 seconds")]
    TimedOut,
}

/// Ping first; when absent, start the daemon and poll the call until it answers.
pub(crate) fn run(
    env: &Environment,
    mut ping: impl FnMut() -> Option<PingResult>,
    err: &mut dyn Write,
) -> Result<PingResult, Error> {
    // The state directory first, as the daemon resolves it, so a refusal reads as the daemon's.
    env.state_dir()?;
    validate_configuration(env)?;
    if let Some(answer) = ping() {
        return validate_answer(env, answer);
    }

    // Not a refusal: only the hires and launches that need a missing variable are refused, and
    // the daemon names it there.
    for (server, variable) in env.declared().missing() {
        let _ = crate::render::warning(
            err,
            &format!(
                "MCP server `{server}` names {variable}, which this shell does not set; the daemon \
                 starts without it and refuses any agent that needs it"
            ),
        );
    }
    let mut child = spawn_daemon(env)?;
    let deadline = Instant::now() + START_DEADLINE;
    loop {
        if let Some(answer) = ping() {
            return match validate_answer(env, answer) {
                Ok(answer) => Ok(answer),
                Err(error) => {
                    stop_spawned(&mut child);
                    Err(error)
                }
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => return Err(Error::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                stop_spawned(&mut child);
                return Err(Error::Wait(error));
            }
        }
        if Instant::now() >= deadline {
            stop_spawned(&mut child);
            return Err(Error::TimedOut);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn validate_configuration(env: &Environment) -> Result<(), Error> {
    // The daemon exits on these two and only its log says why, so say it here first.
    if let Err(refused @ (NoConfig::Empty | NoConfig::Relative)) = env.config() {
        return Err(Error::RefusedConfig(refused));
    }
    let derived = dudes_rpc::local_endpoint(env.runtime_dir()?, SOCKET_NAME);
    if env.socket()? != derived {
        return Err(Error::ConfiguredSocketDiverges {
            configured: env.socket()?.to_path_buf(),
            derived,
        });
    }
    check_socket_path(env.socket()?, true)?;
    Ok(())
}

/// The longest Unix socket path, in bytes, that fits `sun_path` with its trailing NUL.
#[cfg(any(target_os = "linux", target_os = "android"))]
const MAX_SOCKET_PATH: Option<usize> = Some(107);
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const MAX_SOCKET_PATH: Option<usize> = Some(103);
#[cfg(not(unix))]
const MAX_SOCKET_PATH: Option<usize> = None;

/// A socket path longer than the platform's `sun_path`, which no daemon could bind.
#[derive(Debug, thiserror::Error)]
#[error(
    "the daemon's socket path {path} is {length} bytes long, and this platform allows at most \
     {limit}{advice}",
    path = .path.display(),
    advice = if *.derived { ". Use a shorter home directory, or set DUDES_RUNTIME_DIR to a shorter directory" } else { "" }
)]
pub(crate) struct SocketPathTooLong {
    path: PathBuf,
    length: usize,
    limit: usize,
    /// Whether the path comes from the runtime directory, the only case a shorter directory fixes.
    derived: bool,
}

/// Whether `socket` is the one the runtime directory derives, not a `DUDES_SOCK` or configured
/// override.
pub(crate) fn is_derived_socket(socket: &Path, runtime_dir: Option<&Path>) -> bool {
    runtime_dir.is_some_and(|dir| dudes_rpc::local_endpoint(dir, SOCKET_NAME) == socket)
}

pub(crate) fn check_socket_path(path: &Path, derived: bool) -> Result<(), SocketPathTooLong> {
    check_socket_path_against(path, MAX_SOCKET_PATH, derived)
}

fn check_socket_path_against(
    path: &Path,
    limit: Option<usize>,
    derived: bool,
) -> Result<(), SocketPathTooLong> {
    let Some(limit) = limit else { return Ok(()) };
    let length = path.as_os_str().len();
    if length > limit {
        return Err(SocketPathTooLong {
            path: path.to_path_buf(),
            length,
            limit,
            derived,
        });
    }
    Ok(())
}

fn validate_answer(env: &Environment, answer: PingResult) -> Result<PingResult, Error> {
    let endpoint = env.socket()?;
    if answer.socket != endpoint {
        return Err(Error::ReportedSocketDiverges {
            reported: answer.socket,
            endpoint: endpoint.to_path_buf(),
        });
    }
    Ok(answer)
}

fn create_runtime_dir(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::RuntimeDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    create_runtime_dir_leaf(path)
}

#[cfg(unix)]
fn create_runtime_dir_leaf(path: &Path) -> Result<(), Error> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(Error::RuntimeDirectory {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(windows)]
fn create_runtime_dir_leaf(path: &Path) -> Result<(), Error> {
    dudes_win::create_runtime_dir_creator_only(path).map_err(|source| Error::RuntimeDirectory {
        path: path.to_path_buf(),
        source,
    })
}

#[expect(
    clippy::disallowed_types,
    reason = "quickstart creates exactly one process, the daemon; the source gate confines it"
)]
fn spawn_daemon(env: &Environment) -> Result<Child, Error> {
    let daemon = env.daemon().map_err(Error::ResolveDaemon)?;
    if !daemon.is_absolute() {
        return Err(Error::RelativeDaemon(daemon));
    }

    let runtime_dir = env.runtime_dir()?;
    let state_dir = env.state_dir()?;
    create_runtime_dir(runtime_dir)?;
    let log_path = runtime_dir.join(LOG_NAME);
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut log = options.open(&log_path).map_err(|source| Error::Log {
        path: log_path.clone(),
        source,
    })?;
    let stdout = log.try_clone().map_err(|source| Error::Log {
        path: log_path.clone(),
        source,
    })?;
    let stderr = log.try_clone().map_err(|source| Error::Log {
        path: log_path.clone(),
        source,
    })?;

    #[cfg(unix)]
    let mut child = {
        let mut unix_command = std::process::Command::new(&daemon);
        unix_command.env_clear();
        for var in env.daemon_env().iter().chain(env.declared().held()) {
            unix_command.env(var.name(), var.expose_value());
        }
        unix_command.env("DUDES_RUNTIME_DIR", runtime_dir);
        unix_command.env("DUDES_STATE_DIR", state_dir);
        unix_command.stdin(std::process::Stdio::null());
        unix_command.stdout(std::process::Stdio::from(stdout));
        unix_command.stderr(std::process::Stdio::from(stderr));
        unix_command.process_group(0);
        unix_command.spawn().map_err(|source| Error::Spawn {
            path: daemon.clone(),
            source,
        })?
    };
    #[cfg(windows)]
    let mut child = {
        let mut command = windows_spawn::Command::new(&daemon);
        command.env_clear();
        for var in env.daemon_env().iter().chain(env.declared().held()) {
            command.env(var.name(), var.expose_value());
        }
        command.env("DUDES_RUNTIME_DIR", runtime_dir);
        command.env("DUDES_STATE_DIR", state_dir);
        command.stdin(windows_spawn::Stdio::null());
        command.stdout(windows_spawn::Stdio::from(stdout));
        command.stderr(windows_spawn::Stdio::from(stderr));
        let spawn_options = windows_spawn::SpawnOptions::new()
            .creation_flags(
                windows_spawn::CreationFlags::DETACHED_PROCESS
                    | windows_spawn::CreationFlags::NEW_PROCESS_GROUP,
            )
            .drop_policy(windows_spawn::DropPolicy::Detach);
        command
            .spawn_with(spawn_options)
            .map_err(|source| Error::Spawn {
                path: daemon.clone(),
                source,
            })?
    };
    if let Err(source) = write_spawn_marker(&mut log, child.id()) {
        stop_spawned(&mut child);
        return Err(Error::Log {
            path: log_path,
            source,
        });
    }
    Ok(child)
}

/// Write the spawn marker to the daemon's log in **one** call, not three.
///
/// One call because the daemon is appending to this same file through a `try_clone` of the
/// descriptor handed in here. `writeln!` on an unbuffered `File` issues one write per formatting
/// piece — marker, pid, newline — and the daemon's own first block lands between them: the log reads
/// `quickstart started duded pid 55362DUDES_RUNTIME_DIR=/var/…`, which
/// leaves the only record of what quickstart started unparseable and the pid attached to a line
/// that is not the marker.
///
/// **What the single call buys, at the strength it actually holds.** One small `write(2)` to a
/// descriptor opened `O_APPEND` is one atomic append — the seek to the end and the write are not
/// separable, so no other writer's bytes land inside it. That is a property of the one `write(2)`
/// and **not** of `write_all`, which loops on a short write; what makes this one call is that the
/// buffer is tens of bytes and a regular file takes it whole.
///
/// The sink is a `Write` rather than the log file itself so that the call count is observable:
/// `tests::the_spawn_marker_is_written_in_one_call` counts calls against a sink that takes every
/// buffer whole, which is the arm `writeln!` fails and this passes.
fn write_spawn_marker(sink: &mut impl Write, pid: u32) -> io::Result<()> {
    sink.write_all(format!("{SPAWN_MARKER}{pid}\n").as_bytes())
}

fn stop_spawned(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::write_spawn_marker;

    #[cfg(unix)]
    #[test]
    fn a_socket_path_one_byte_over_the_platform_limit_is_refused() {
        use super::{MAX_SOCKET_PATH, check_socket_path};

        let limit = MAX_SOCKET_PATH.expect("a Unix socket path has a limit");
        let at_limit = std::path::PathBuf::from(format!("/{}", "a".repeat(limit - 1)));
        assert_eq!(at_limit.as_os_str().len(), limit);
        check_socket_path(&at_limit, true).expect("the longest path fits");

        let over = std::path::PathBuf::from(format!("/{}", "a".repeat(limit)));
        let refused = check_socket_path(&over, true).expect_err("one byte more is refused");
        let message = refused.to_string();
        assert!(
            message.contains(&format!("{} bytes", limit + 1)),
            "{message}"
        );
        assert!(message.contains(&format!("at most {limit}")), "{message}");
        assert!(message.contains(&over.display().to_string()), "{message}");
        assert!(message.contains("DUDES_RUNTIME_DIR"), "{message}");

        let overridden = check_socket_path(&over, false).expect_err("an override is refused too");
        let message = overridden.to_string();
        assert!(message.contains(&format!("at most {limit}")), "{message}");
        assert!(!message.contains("DUDES_RUNTIME_DIR"), "{message}");
    }

    /// A sink that takes every buffer whole and counts the calls it took them in.
    ///
    /// Taking each buffer whole is the point: `write_all` loops only on a short write, so a sink
    /// that never returns one turns *how many calls* into *how many pieces the formatting
    /// produced* — one for a marker formatted then written, three for `writeln!`.
    #[derive(Default)]
    struct CountingSink {
        calls: usize,
        written: Vec<u8>,
    }

    impl Write for CountingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The marker reaches the log in one call, carrying the pid on the marker's own line.
    ///
    /// **This is a fail-side control that needs no timing at all.** Written as
    /// `writeln!(sink, "{SPAWN_MARKER}{pid}")` the count is three and the first assertion fails on
    /// every run, whereas the interleaving itself is a race that a timing-based check would catch
    /// only by luck.
    #[test]
    fn the_spawn_marker_is_written_in_one_call() {
        let mut sink = CountingSink::default();

        write_spawn_marker(&mut sink, 55362).expect("this sink takes every buffer whole");

        assert_eq!(
            sink.calls, 1,
            "the marker must be one append: another writer's block lands between two calls"
        );
        assert_eq!(
            String::from_utf8(sink.written).expect("the marker is UTF-8"),
            "quickstart started duded pid 55362\n",
            "and it carries the pid on the marker's own line, newline included — written out \
             literally, never rebuilt from `SPAWN_MARKER`, so a changed marker is visible here"
        );
    }
}
