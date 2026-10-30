use std::io::{self, Write};

use crate::args::MailCommand;
use crate::env::Environment;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;

pub(crate) fn run(command: &MailCommand, env: &Environment, out: &mut dyn Write) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        unix::run(
            command,
            env.mailbox().map_err(io::Error::other)?,
            env.scoped_file_mail(),
            out,
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (command, env, out);
        Err(io::Error::other("mail is not yet available on Windows"))
    }
}

/// Queues this agent's closing acknowledgement in its own outbox.
pub(crate) fn closing_ack(
    announcement: dudes_types::Lsn,
    env: &Environment,
    out: &mut dyn Write,
) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        unix::closing_ack(announcement, env.mailbox().map_err(io::Error::other)?, out)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (announcement, env, out);
        Err(io::Error::other("mail is not yet available on Windows"))
    }
}
