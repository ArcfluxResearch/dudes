//! Hand a Floor link to the system's opener, which puts it in the default browser.
//!
//! The link is a bearer for sixty seconds and for one use. It goes to the opener as one argument
//! of a typed argv, never through a shell, and nowhere else: the opener's own output is dropped.
//! A copy that stays in a long-lived browser's argument list is already spent by then, because the
//! page exchanges the link as it loads.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::env::Environment;

/// How long to wait for the opener to hand the link over before assuming it has.
const HANDOFF: Duration = Duration::from_secs(5);

/// What came of trying to open a link.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    /// The opener took the link.
    Opened,
    /// Nothing was tried, for the reason given; the link has to be shown instead.
    Skipped(&'static str),
    /// The opener could not be started, or refused the link.
    Failed(String),
}

/// Open `url` with the opener `DUDES_OPENER` names, else the system's.
pub(crate) fn launch(env: &Environment, url: &str) -> Launch {
    if over_ssh() {
        return Launch::Skipped(
            "this is an SSH session, so a browser on this machine would not be yours",
        );
    }
    let program = match env.opener() {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(_) => return Launch::Failed("DUDES_OPENER must be an absolute path".to_owned()),
        None => match system_opener() {
            Some(path) => path,
            None => return Launch::Skipped("no program to open links was found"),
        },
    };
    run(&program, url)
}

fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// The opener the platform ships, at a fixed absolute path: this CLI searches no `PATH` for it.
fn system_opener() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/usr/bin/open"]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/bin/xdg-open",
            "/usr/local/bin/xdg-open",
            "/bin/xdg-open",
        ]
    } else {
        &[]
    };
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

#[expect(
    clippy::disallowed_types,
    reason = "the link goes to the operator's chosen opener as one typed argument"
)]
fn run(program: &Path, url: &str) -> Launch {
    let mut child = match std::process::Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Launch::Failed(format!("could not start {}: {error}", program.display()));
        }
    };
    let deadline = Instant::now() + HANDOFF;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Launch::Opened,
            Ok(Some(status)) => {
                return Launch::Failed(format!("{} exited with {status}", program.display()));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            // Some openers stay until the browser they started exits; the link is handed over.
            Ok(None) => return Launch::Opened,
            Err(error) => return Launch::Failed(format!("waiting for the opener: {error}")),
        }
    }
}
