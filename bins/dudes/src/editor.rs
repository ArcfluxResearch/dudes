//! Hand a ticket's recorded checkout to the operator's selected editor.

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::path::{Path, PathBuf};

use dudes_proto::rpc::ticket::TicketCheckoutResult;

use crate::args::EditorSelection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrintDisposition {
    DirectoryOnly,
    LaunchForm,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EditorError {
    #[error("The recorded checkout directory is no longer available.")]
    CheckoutUnavailable,
    #[cfg(unix)]
    #[error("The editor must name an absolute program.")]
    RelativeProgram,
    #[cfg(unix)]
    #[error("The editor is not an executable file.")]
    NotExecutable,
    #[cfg(unix)]
    #[error("The editor must be outside the recorded checkout.")]
    InsideCheckout,
    #[cfg(unix)]
    #[error("The selected editor configuration is not valid UTF-8.")]
    UnsupportedEncoding,
    #[cfg(unix)]
    #[error("Could not start the editor: {0}")]
    Exec(#[source] std::io::Error),
}

pub(crate) fn handoff(
    checkout: &TicketCheckoutResult,
    selection: &EditorSelection,
    machine: bool,
) -> Result<PrintDisposition, EditorError> {
    if !checkout.directory.is_absolute() || !checkout.directory.is_dir() {
        return Err(EditorError::CheckoutUnavailable);
    }
    if machine || matches!(selection, EditorSelection::Print) {
        return Ok(PrintDisposition::DirectoryOnly);
    }

    #[cfg(not(unix))]
    {
        Ok(PrintDisposition::DirectoryOnly)
    }
    #[cfg(unix)]
    {
        if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return Ok(PrintDisposition::DirectoryOnly);
        }
        let (program, args) = match selection {
            EditorSelection::Print => return Ok(PrintDisposition::DirectoryOnly),
            EditorSelection::Explicit(program) => (program.clone(), Vec::new()),
            EditorSelection::Configured => {
                let Some((program, args)) = configured_editor()? else {
                    return Ok(PrintDisposition::LaunchForm);
                };
                (resolve_configured_program(program)?, args)
            }
        };
        if !program.is_absolute() {
            return Err(EditorError::RelativeProgram);
        }
        let program = program
            .canonicalize()
            .map_err(|_| EditorError::NotExecutable)?;
        if !runnable(&program) {
            return Err(EditorError::NotExecutable);
        }
        let directory = checkout
            .directory
            .canonicalize()
            .map_err(|_| EditorError::CheckoutUnavailable)?;
        if program.starts_with(directory) {
            return Err(EditorError::InsideCheckout);
        }
        Err(EditorError::Exec(exec(
            &program,
            &args,
            &checkout.directory,
        )))
    }
}

#[cfg(unix)]
fn configured_editor() -> Result<Option<(PathBuf, Vec<OsString>)>, EditorError> {
    for name in ["VISUAL", "EDITOR"] {
        let value = match std::env::var(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => continue,
            Err(std::env::VarError::NotUnicode(_)) => return Err(EditorError::UnsupportedEncoding),
        };
        let mut tokens = value.split_whitespace();
        if let Some(program) = tokens.next() {
            return Ok(Some((
                PathBuf::from(program),
                tokens.map(OsString::from).collect(),
            )));
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn resolve_configured_program(program: PathBuf) -> Result<PathBuf, EditorError> {
    if program.is_absolute() {
        return Ok(program);
    }
    if program.file_name() != Some(program.as_os_str()) {
        return Err(EditorError::RelativeProgram);
    }
    let path = std::env::var_os("PATH").ok_or(EditorError::NotExecutable)?;
    let cwd = std::env::current_dir();
    for entry in std::env::split_paths(&path) {
        let entry = if entry.is_absolute() {
            entry
        } else {
            cwd.as_ref()
                .map_err(|_| EditorError::NotExecutable)?
                .join(entry)
        };
        let candidate = entry.join(&program);
        if runnable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(EditorError::NotExecutable)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn runnable(candidate: &Path) -> bool {
    candidate.is_file() && rustix::fs::access(candidate, rustix::fs::Access::EXEC_OK).is_ok()
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn runnable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    candidate
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "the operator's selected editor replaces this process with the recorded directory"
)]
fn exec(program: &Path, args: &[OsString], directory: &Path) -> std::io::Error {
    let mut command = std::process::Command::new(program);
    command.args(args);
    command.arg(directory);
    command.env_remove("DUDES_TOKEN");
    command.env_remove("DUDES_SOCK");
    command.exec()
}
