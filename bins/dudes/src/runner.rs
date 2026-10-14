//! Run one program to completion, within a time limit, and capture what it printed.
//!
//! This module creates the only process `dudes upgrade` starts, twice over: the system `curl`
//! and the release installer, each an absolute path with a typed argument vector, a cleared
//! environment and no standard input. Nothing is interpolated into a command string. The child
//! leads its own process group, so a program that outlives its limit is stopped with everything
//! it started.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::Duration;

use dudes_providers::launch::EnvVar;

/// What a finished program left behind.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) timed_out: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run `program` with exactly `args` and `env`, and stop it if it is still running after `limit`.
#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "upgrade runs the system curl and the release installer; the source gate confines it"
)]
pub(crate) fn run(
    program: &Path,
    args: &[OsString],
    env: &[EnvVar],
    limit: Duration,
) -> io::Result<Output> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    use std::time::Instant;

    let mut command = std::process::Command::new(program);
    command.env_clear();
    for var in env {
        command.env(var.name(), var.expose_value());
    }
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn()?;
    let drain = |stream: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let deadline = Instant::now() + limit;
    let (success, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status.success(), false);
        }
        if Instant::now() >= deadline {
            stop_group(&mut child);
            break (false, true);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        success,
        timed_out,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Ask the child's process group to end, and make it end if it does not.
#[cfg(unix)]
fn stop_group(child: &mut std::process::Child) {
    use rustix::process::{Pid, Signal, kill_process_group};
    use std::time::Instant;

    let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::TERM);
        let grace = Instant::now() + Duration::from_secs(5);
        while Instant::now() < grace && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
pub(crate) fn run(
    _program: &Path,
    _args: &[OsString],
    _env: &[EnvVar],
    _limit: Duration,
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "upgrade is not supported on this platform",
    ))
}

#[cfg(test)]
mod tests;
