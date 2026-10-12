//! Hand a ticket's recorded checkout to the operator's explicit editor.

#[cfg(unix)]
use std::path::Path;

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
    if machine {
        return Ok(PrintDisposition::DirectoryOnly);
    }
    match selection {
        EditorSelection::Print => Ok(PrintDisposition::DirectoryOnly),
        EditorSelection::Configured => Ok(PrintDisposition::LaunchForm),
        #[cfg(not(unix))]
        EditorSelection::Explicit(_) => Ok(PrintDisposition::DirectoryOnly),
        #[cfg(unix)]
        EditorSelection::Explicit(program) => {
            use std::os::unix::fs::PermissionsExt as _;

            if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
                .iter()
                .any(|name| std::env::var_os(name).is_some())
            {
                return Ok(PrintDisposition::DirectoryOnly);
            }
            if !program.is_absolute() {
                return Err(EditorError::RelativeProgram);
            }
            let program = program
                .canonicalize()
                .map_err(|_| EditorError::NotExecutable)?;
            let metadata = program.metadata().map_err(|_| EditorError::NotExecutable)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(EditorError::NotExecutable);
            }
            let directory = checkout
                .directory
                .canonicalize()
                .map_err(|_| EditorError::CheckoutUnavailable)?;
            if program.starts_with(directory) {
                return Err(EditorError::InsideCheckout);
            }
            Err(EditorError::Exec(exec(&program, &checkout.directory)))
        }
    }
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "the operator's explicit editor replaces this process with the recorded directory"
)]
fn exec(program: &Path, directory: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new(program);
    command.arg(directory);
    command.env_remove("DUDES_TOKEN");
    command.env_remove("DUDES_SOCK");
    command.exec()
}
