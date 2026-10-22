//! The system's folder dialog, opened by the daemon for the Floor's Add project and Hire.
//!
//! A browser page cannot open a dialog on the daemon's machine, and the daemon runs on the same
//! machine as the browser, so the daemon opens it. The dialog is a child of the daemon, run through
//! the one-shot runner every other short-lived child uses: a typed argument list with nothing from
//! the page in it, a deadline after which the child is ended, and the daemon's exit sweeps it. One
//! dialog is open at a time. The answer is only a path the page then previews, so choosing a folder
//! grants nothing that typing it does not.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};

use crate::predicate::{PredicateCaptured, PredicateRunner};

/// How long a dialog may stay open before the daemon ends it.
pub const DIALOG_DEADLINE: Duration = Duration::from_mins(5);

/// The most output read from a dialog: a path is far shorter.
const OUTPUT_CAP: usize = 8192;

/// What `osascript` is asked to show, and nothing else.
const MACOS_PROGRAM: &str = "/usr/bin/osascript";
const MACOS_SCRIPT: &str = "POSIX path of (choose folder with prompt \"Choose a project folder\")";

/// The variables a dialog needs to find the user's screen, session and language.
const DIALOG_ENV: [&str; 12] = [
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// What a request for a folder came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The absolute path the person chose.
    Path(String),
    /// The person closed the dialog, or it stayed open until the deadline.
    Cancelled,
    /// Another dialog is open.
    Busy,
    /// No dialog can be shown here, or it failed.
    Unavailable,
}

/// The platforms the picker knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// macOS: the system's own `osascript`.
    Macos,
    /// Linux: `zenity` or `kdialog`, with a display.
    Linux,
    /// Anywhere else: no picker.
    Other,
}

impl Platform {
    /// The platform this daemon was built for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

/// What the daemon's environment says about where a dialog would appear.
#[derive(Clone, Debug, Default)]
struct Surroundings {
    /// A remote login: the dialog would open on a screen nobody is looking at.
    remote: bool,
    /// A display server the daemon can reach.
    display: bool,
}

impl Surroundings {
    fn of(daemon_env: &[EnvVar]) -> Self {
        let set = |name: &str| {
            daemon_env
                .iter()
                .any(|var| var.name() == OsStr::new(name) && !var.expose_value().is_empty())
        };
        Self {
            remote: set("SSH_CONNECTION") || set("SSH_TTY"),
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
        }
    }
}

/// The program and arguments that open the dialog on `platform`, or `None` where there is none.
fn command(
    platform: Platform,
    surroundings: &Surroundings,
    path: Option<&OsStr>,
) -> Option<Vec<OsString>> {
    if surroundings.remote {
        return None;
    }
    match platform {
        Platform::Macos => Some(
            [MACOS_PROGRAM, "-e", "activate", "-e", MACOS_SCRIPT]
                .map(OsString::from)
                .to_vec(),
        ),
        Platform::Linux if surroundings.display => {
            let found = |name: &str| crate::program::on_path(OsStr::new(name), path);
            if let Some(zenity) = found("zenity") {
                Some(vec![
                    zenity.into_os_string(),
                    "--file-selection".into(),
                    "--directory".into(),
                    "--title=Choose a project folder".into(),
                ])
            } else {
                found("kdialog")
                    .map(|kdialog| vec![kdialog.into_os_string(), "--getexistingdirectory".into()])
            }
        }
        Platform::Linux | Platform::Other => None,
    }
}

/// The folder dialog, shared by every Floor connection.
pub struct FolderPicker {
    runner: Arc<dyn PredicateRunner>,
    platform: Platform,
    surroundings: Surroundings,
    path: Option<OsString>,
    env: Vec<EnvVar>,
    open: AtomicBool,
    #[cfg(feature = "testing")]
    stand_in: std::sync::Mutex<Option<(Vec<OsString>, Duration)>>,
}

impl std::fmt::Debug for FolderPicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderPicker")
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

/// Closes the picker for the next request when the dialog ends, however it ends.
struct Open<'a>(&'a AtomicBool);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl FolderPicker {
    /// A picker for the daemon with `daemon_env`, finding `zenity` or `kdialog` along `path`.
    #[must_use]
    pub fn new(
        runner: Arc<dyn PredicateRunner>,
        daemon_env: &[EnvVar],
        path: Option<OsString>,
    ) -> Self {
        Self::on(Platform::current(), runner, daemon_env, path)
    }

    fn on(
        platform: Platform,
        runner: Arc<dyn PredicateRunner>,
        daemon_env: &[EnvVar],
        path: Option<OsString>,
    ) -> Self {
        let env = daemon_env
            .iter()
            .filter(|var| DIALOG_ENV.iter().any(|name| var.name() == OsStr::new(name)))
            .cloned()
            .collect();
        Self {
            runner,
            platform,
            surroundings: Surroundings::of(daemon_env),
            path,
            env,
            open: AtomicBool::new(false),
            // A binary built with the `testing` feature takes its stand-in from the environment, so a
            // proof can run a whole daemon without opening a real dialog.
            #[cfg(feature = "testing")]
            stand_in: std::sync::Mutex::new(
                daemon_env
                    .iter()
                    .find(|var| var.name() == OsStr::new("DUDES_TEST_FOLDER_PICKER"))
                    .map(|var| (vec![var.expose_value().to_owned()], DIALOG_DEADLINE)),
            ),
        }
    }

    /// Replace the dialog with `argv`, ended after `deadline`. Tests only: no release build has
    /// this, so no page or configuration can name another program.
    #[cfg(feature = "testing")]
    pub fn use_stand_in(&self, argv: Vec<OsString>, deadline: Duration) {
        *self
            .stand_in
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((argv, deadline));
    }

    /// The program that opens the dialog and how long it may stay open, or `None` where no dialog
    /// can be shown.
    fn plan(&self) -> Option<(Vec<OsString>, Duration)> {
        #[cfg(feature = "testing")]
        if let Some((argv, deadline)) = self
            .stand_in
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return (!self.surroundings.remote).then_some((argv, deadline));
        }
        command(self.platform, &self.surroundings, self.path.as_deref())
            .map(|argv| (argv, DIALOG_DEADLINE))
    }

    /// Whether a dialog can be shown here. The page offers Choose folder only when it can.
    #[must_use]
    pub fn available(&self) -> bool {
        self.plan().is_some()
    }

    /// Open the dialog and wait for the person's choice. Blocks the calling thread until the
    /// dialog ends.
    #[must_use]
    pub fn choose(&self) -> Choice {
        let Some((argv, deadline)) = self.plan() else {
            return Choice::Unavailable;
        };
        if self.open.swap(true, Ordering::AcqRel) {
            return Choice::Busy;
        }
        let _open = Open(&self.open);
        let spec = LaunchSpec {
            argv,
            env: self.env.clone(),
            cwd: std::env::temp_dir(),
            stdin: LaunchInput::Null,
        };
        match self.runner.run_captured(&spec, deadline, OUTPUT_CAP) {
            Ok(PredicateCaptured::Finished {
                timed_out: true, ..
            }) => Choice::Cancelled,
            Ok(PredicateCaptured::Finished {
                exit: Some(0),
                stdout,
                truncated: false,
                ..
            }) => path_of(&stdout).map_or(Choice::Unavailable, Choice::Path),
            // The dialogs exit 1 when the person cancels. `osascript` also exits 1 for any script
            // error, and its message is not read, so an error there reads as a cancel.
            Ok(PredicateCaptured::Finished {
                exit: Some(1),
                stdout,
                ..
            }) if stdout.is_empty() => Choice::Cancelled,
            _ => Choice::Unavailable,
        }
    }
}

/// The one absolute path a dialog printed, without its closing line break.
fn path_of(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    let path = text.strip_suffix('\n').unwrap_or(text);
    (PathBuf::from(path).is_absolute() && !path.chars().any(char::is_control))
        .then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<EnvVar> {
        pairs
            .iter()
            .map(|(name, value)| EnvVar::new(*name, *value))
            .collect()
    }

    #[test]
    fn macos_runs_exactly_the_fixed_script() {
        let argv = command(Platform::Macos, &Surroundings::of(&[]), None).expect("a dialog");
        assert_eq!(
            argv,
            [
                "/usr/bin/osascript",
                "-e",
                "activate",
                "-e",
                "POSIX path of (choose folder with prompt \"Choose a project folder\")"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn a_remote_login_has_no_dialog() {
        for variable in ["SSH_CONNECTION", "SSH_TTY"] {
            let here = Surroundings::of(&env(&[(variable, "x"), ("DISPLAY", ":0")]));
            assert_eq!(command(Platform::Macos, &here, None), None, "{variable}");
            assert_eq!(command(Platform::Linux, &here, None), None, "{variable}");
        }
    }

    #[test]
    fn linux_needs_a_display_and_a_program() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = std::env::join_paths([directory.path()]).expect("a path");
        let displayed = Surroundings::of(&env(&[("WAYLAND_DISPLAY", "wayland-0")]));
        let headless = Surroundings::of(&[]);
        assert_eq!(
            command(Platform::Linux, &displayed, Some(&path)),
            None,
            "no program on the path"
        );
        for name in ["kdialog", "zenity"] {
            let file = directory.path().join(name);
            std::fs::write(&file, b"#!/bin/sh\n").expect("a program");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                    .expect("executable");
            }
        }
        let argv = command(Platform::Linux, &displayed, Some(&path)).expect("a dialog");
        assert!(
            argv[0].to_string_lossy().ends_with("zenity"),
            "zenity comes first: {argv:?}"
        );
        assert_eq!(
            &argv[1..],
            [
                "--file-selection",
                "--directory",
                "--title=Choose a project folder"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            command(Platform::Linux, &headless, Some(&path)),
            None,
            "no display"
        );
        assert_eq!(command(Platform::Other, &displayed, Some(&path)), None);
    }

    #[test]
    fn only_one_absolute_path_with_one_line_break_is_a_choice() {
        assert_eq!(
            path_of(b"/work/kim/app/\n").as_deref(),
            Some("/work/kim/app/")
        );
        assert_eq!(path_of(b"/tmp/a").as_deref(), Some("/tmp/a"));
        for junk in [
            &b""[..],
            b"\n",
            b"relative/path\n",
            b"/tmp/a\n\n",
            b"/tmp/a\nb\n",
            b"\xff/tmp\n",
            b"/tmp/\x1b[2J\n",
        ] {
            assert_eq!(path_of(junk), None, "{junk:?}");
        }
    }
}
