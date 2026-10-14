//! Run one program to completion and capture what it printed.
//!
//! This module creates the only process `dudes upgrade` starts, twice over: the system `curl`
//! and the release's installer, each an absolute path with a typed argument vector, a cleared
//! environment and no standard input. Nothing is interpolated into a command string.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// What a finished program left behind.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run `program` with exactly `args` and `env`.
#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "upgrade runs the system curl and the release installer; the source gate confines it"
)]
pub(crate) fn run(
    program: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
) -> io::Result<Output> {
    let mut command = std::process::Command::new(program);
    command.env_clear();
    command.envs(env.iter().map(|(name, value)| (name, value)));
    command.args(args);
    command.stdin(std::process::Stdio::null());
    let output = command.output()?;
    Ok(Output {
        success: output.status.success(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(not(unix))]
pub(crate) fn run(
    _program: &Path,
    _args: &[OsString],
    _env: &[(OsString, OsString)],
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "upgrade is not supported on this platform",
    ))
}
