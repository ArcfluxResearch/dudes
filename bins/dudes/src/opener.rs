//! Hand a Floor link to the system's opener, which puts it in the default browser.
//!
//! The link is a bearer for sixty seconds and for one use. It goes to the opener as one argument
//! of a typed argv, never through a shell, and nowhere else: the opener's own output is dropped.
//! A copy that stays in a long-lived browser's argument list is already spent by then, because the
//! page exchanges the link as it loads.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

use crate::env::Environment;

/// How long to wait for the opener to fail before counting the link as handed over. An opener
/// still running then is left alone: it may be waiting on a browser it just started, and the
/// browser must outlive this command.
#[cfg(unix)]
const HANDOFF: Duration = Duration::from_secs(2);

/// What the opener needs to find a browser and a display, and nothing else: the rest of this
/// process's environment, including any agent token, stays behind.
#[cfg(unix)]
const OPENER_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "LANG",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "BROWSER",
];

/// What came of trying to open a link.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    /// The opener took the link.
    #[cfg_attr(not(unix), expect(dead_code, reason = "only the Unix opener runs"))]
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

/// Start the opener and wait briefly for it to fail. It is never signalled: it leads a process
/// group of its own, its three standard streams are `/dev/null`, and it is dropped without a wait.
#[cfg(unix)]
fn run(program: &Path, url: &str) -> Launch {
    let mut child = match spawn(program, url) {
        Ok(child) => child,
        Err(error) => {
            return Launch::Failed(format!("could not start {}: {error}", program.display()));
        }
    };
    let deadline = std::time::Instant::now() + HANDOFF;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Launch::Opened,
            Ok(Some(_)) => {
                return Launch::Failed(format!("{} refused the link", program.display()));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => return Launch::Opened,
            Err(error) => return Launch::Failed(format!("waiting for the opener: {error}")),
        }
    }
}

#[cfg(not(unix))]
fn run(_program: &Path, _url: &str) -> Launch {
    Launch::Skipped("opening a browser is not supported on this system")
}

/// The one process this module creates: an absolute program, the link as its only argument, an
/// environment cleared down to [`OPENER_ENV`], no standard input and no output.
#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "the link goes to the operator's opener as one typed argument; the source gate confines it"
)]
fn spawn(program: &Path, url: &str) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let mut command = std::process::Command::new(program);
    command.env_clear();
    for name in OPENER_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.arg(url);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.process_group(0);
    command.spawn()
}
