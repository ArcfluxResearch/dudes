//! `dudes upgrade`: find the latest release, and install it in place with the release's own
//! installer.
//!
//! This is the only place a Dudes program connects to GitHub, and only when the operator runs it.
//! The connection is made by the system `curl` at an absolute path, never by this binary, and the
//! installer's checksum check and restore logic do the rest. Nothing checks for updates on its own.
//!
//! The processes and the daemon are reached through [`Runner`] and [`Restart`], so everything here
//! is drivable without a network or a daemon.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dudes_proto::rpc::floor::SessionExport;
use dudes_providers::launch::EnvVar;

use crate::args::UpgradeOptions;
use crate::exit;
use crate::layout::installed_prefix;
use crate::lifecycle::{self, Daemon};
use crate::render::{self, Escaped};
use crate::runner::Output;

/// The system `curl`. An absolute path, so no `PATH` search decides which program runs.
pub(crate) const CURL: &str = "/usr/bin/curl";

const INSTALL_SH: &str = "/bin/sh";
const REPOSITORY: &str = "https://github.com/ArcfluxResearch/dudes";
const RAW: &str = "https://raw.githubusercontent.com/ArcfluxResearch/dudes";
const ONE_LINER: &str =
    "curl -fsSL https://raw.githubusercontent.com/ArcfluxResearch/dudes/main/install.sh | sh";
const REBUILD: &str =
    "https://github.com/ArcfluxResearch/dudes/blob/main/docs/building-from-source.md";

/// How long a lookup or a download may take.
const FETCH_LIMIT: Duration = Duration::from_secs(150);
/// How long the installer may take, including its own download of the release.
const INSTALL_LIMIT: Duration = Duration::from_secs(600);

/// A release version: `major.minor.patch` and an optional pre-release, as Cargo writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    text: String,
    core: [u64; 3],
    pre: Vec<Identifier>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Version {
    /// Parse `text`, refusing anything that is not a plain version.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = core.split('.').map(number);
        let version = Self {
            text: text.to_owned(),
            core: [numbers.next()??, numbers.next()??, numbers.next()??],
            pre: match pre {
                None => Vec::new(),
                Some(pre) => pre
                    .split('.')
                    .map(|part| {
                        if part.is_empty()
                            || !part
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                        {
                            None
                        } else {
                            Some(number(part).map_or_else(
                                || Identifier::Text(part.to_owned()),
                                Identifier::Numeric,
                            ))
                        }
                    })
                    .collect::<Option<Vec<_>>>()?,
            },
        };
        numbers.next().is_none().then_some(version)
    }
}

fn number(part: &str) -> Option<u64> {
    let canonical = part == "0" || !part.starts_with('0');
    (canonical && !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| part.parse().ok())?
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Run a program within a time limit and capture what it printed.
pub(crate) trait Runner {
    fn run(
        &self,
        program: &Path,
        args: &[OsString],
        env: &[EnvVar],
        limit: Duration,
    ) -> std::io::Result<Output>;
}

/// What `upgrade` needs beyond the shared daemon: to start the new one.
pub(crate) trait Restart: Daemon {
    /// Start it the way `dudes start` does, and return the version that answers.
    fn start(&self, err: &mut dyn Write) -> Result<String, String>;
    /// The running daemon's Floor sign-ins, to hold in memory across the restart.
    fn export_sign_ins(&self) -> Result<SessionExport, String>;
    /// Hand them to the daemon that answers now, and return how many it kept.
    fn import_sign_ins(&self, exported: &SessionExport) -> Result<u32, String>;
}

/// Where this copy lives and what it may use.
pub(crate) struct Inputs {
    pub(crate) windows: bool,
    pub(crate) current: String,
    pub(crate) executable: PathBuf,
    pub(crate) temp_dir: PathBuf,
    pub(crate) child_env: Vec<EnvVar>,
    pub(crate) state_dir: PathBuf,
    pub(crate) lock_wait: Duration,
    pub(crate) daemon_override: Option<PathBuf>,
}

fn arguments(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// The arguments that read where `releases/latest` redirects, without following it. `-q` comes
/// first so a `.curlrc` cannot change what follows.
pub(crate) fn latest_arguments() -> Vec<OsString> {
    let url = format!("{REPOSITORY}/releases/latest");
    arguments(&[
        "-q",
        "--proto",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--max-time",
        "30",
        "--output",
        "/dev/null",
        "--write-out",
        "%{redirect_url}",
        &url,
    ])
}

/// The arguments that download the installer of release `version` into `file`.
pub(crate) fn download_arguments(version: &str, file: &Path) -> Vec<OsString> {
    let url = format!("{RAW}/v{version}/install.sh");
    let mut args = arguments(&[
        "-q",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--max-redirs",
        "5",
        "--max-time",
        "120",
        "--max-filesize",
        "1048576",
        "--output",
    ]);
    args.push(file.into());
    args.push(url.into());
    args
}

/// The arguments that run the downloaded installer for `version` into `prefix`.
pub(crate) fn install_arguments(file: &Path, version: &str, prefix: &Path) -> Vec<OsString> {
    vec![
        file.into(),
        "--version".into(),
        version.into(),
        "--prefix".into(),
        prefix.into(),
    ]
}

/// The version named by the redirect to the latest release's page.
fn latest_from(redirect: &str) -> Option<Version> {
    Version::parse(
        redirect
            .trim()
            .strip_prefix(&format!("{REPOSITORY}/releases/tag/v"))?,
    )
}

/// A fresh directory only this user can enter, removed when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create(base: &Path) -> std::io::Result<Self> {
        use std::fs::DirBuilder;
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        if !base.is_absolute() {
            return Err(std::io::Error::other(format!(
                "the temporary directory {} is not an absolute path",
                base.display()
            )));
        }
        for _ in 0..8 {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).map_err(std::io::Error::other)?;
            let path = base.join(format!("dudes-upgrade-{:016x}", u64::from_le_bytes(random)));
            #[cfg(unix)]
            let builder = {
                let mut builder = DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = DirBuilder::new();
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other(
            "no unused name for a temporary directory",
        ))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// A failure: something went wrong that is not a refusal.
fn failed(err: &mut dyn Write, message: &str) -> u8 {
    let _ = render::local_error(err, message);
    exit::OTHER
}

/// A deliberate refusal: nothing was changed.
fn refused(err: &mut dyn Write, message: &str) -> u8 {
    let _ = render::local_error(err, message);
    exit::REFUSED
}

fn run_curl(context: &Context<'_>, args: &[OsString], limit: Duration) -> Result<Output, String> {
    context
        .runner
        .run(Path::new(CURL), args, &context.inputs.child_env, limit)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{CURL} was not found. Run the installer yourself, then dudes stop and dudes start: {ONE_LINER}"
                )
            } else {
                format!("could not run {CURL}: {error}")
            }
        })
}

fn latest_release(context: &Context<'_>) -> Result<Version, String> {
    let output = run_curl(context, &latest_arguments(), FETCH_LIMIT)?;
    if !output.success {
        return Err(format!(
            "could not read the latest release from GitHub: {}",
            text(&output.stderr)
        ));
    }
    latest_from(&text(&output.stdout)).ok_or_else(|| {
        "GitHub did not name a latest release in a form this dudes understands".to_owned()
    })
}

#[derive(Clone, Copy)]
struct Context<'a> {
    inputs: &'a Inputs,
    runner: &'a dyn Runner,
    daemon: &'a dyn Restart,
}

/// Why the daemon, if one is installed elsewhere than here, blocks an upgrade.
fn foreign_daemon(inputs: &Inputs, prefix: &Path) -> Option<String> {
    let chosen = inputs.daemon_override.as_ref()?;
    let ours = prefix.join("bin/duded");
    let same = chosen == &ours
        || chosen
            .canonicalize()
            .ok()
            .zip(ours.canonicalize().ok())
            .is_some_and(|(chosen, ours)| chosen == ours);
    (!same).then(|| {
        format!(
            "DUDES_DAEMON is set to {}, not {}, so a restart would not start the daemon this upgrade installs. Unset it, or run the installer yourself",
            chosen.display(),
            ours.display()
        )
    })
}

/// Run `dudes upgrade`. Returns the process's exit code.
pub(crate) fn run(
    inputs: &Inputs,
    options: UpgradeOptions,
    runner: &dyn Runner,
    daemon: &dyn Restart,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    if inputs.windows {
        return refused(
            err,
            &format!(
                "dudes upgrade is not supported on Windows yet; see the install steps in {REBUILD}"
            ),
        );
    }
    let Some(current) = Version::parse(&inputs.current) else {
        return failed(
            err,
            &format!(
                "this dudes reports version {}, which is not a release version",
                inputs.current
            ),
        );
    };
    let prefix = match installed_prefix(&inputs.executable) {
        Ok(prefix) => prefix,
        Err(reason) => {
            return refused(
                err,
                &format!(
                    "{reason}, so dudes upgrade will not replace it. If you built it from source, rebuild it: {REBUILD}"
                ),
            );
        }
    };
    let context = Context {
        inputs,
        runner,
        daemon,
    };
    let running = !options.check && daemon.running();
    if !options.check {
        if let Some(reason) = foreign_daemon(inputs, &prefix) {
            return refused(err, &format!("{reason}. Nothing was changed."));
        }
        if running {
            match lifecycle::check_busy(daemon, options.now, out) {
                Ok(None) => {}
                Ok(Some(message)) | Err(message) => {
                    return refused(err, &format!("refusing to upgrade: {message}"));
                }
            }
        } else if lifecycle::lock_held(&inputs.state_dir) {
            return refused(
                err,
                &format!(
                    "refusing to upgrade: a process holds the lock in {}, so a daemon is running that does not answer; stop it with dudes stop. Nothing was changed.",
                    inputs.state_dir.display()
                ),
            );
        }
    }
    let latest = match latest_release(&context) {
        Ok(latest) => latest,
        Err(message) => return failed(err, &message),
    };
    let _ = writeln!(out, "Current version: {}", Escaped(&inputs.current));
    let _ = writeln!(out, "Latest release:  {}", latest.text);
    if latest <= current {
        let message = if latest < current {
            "This dudes is newer than the latest release. Nothing was changed."
        } else {
            "Dudes is up to date. Nothing was changed."
        };
        let _ = writeln!(out, "{message}");
        return exit::SUCCESS;
    }
    if options.check {
        let _ = writeln!(
            out,
            "An upgrade is available. Run dudes upgrade to install it."
        );
        return exit::SUCCESS;
    }
    match install(&context, &latest.text, &prefix, running, out, err) {
        Ok(code) => code,
        Err(message) => failed(err, &message),
    }
}

fn install(
    context: &Context<'_>,
    latest: &str,
    prefix: &Path,
    running: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let inputs = context.inputs;
    let directory = PrivateDir::create(&inputs.temp_dir)
        .map_err(|error| format!("could not create a private temporary directory: {error}"))?;
    let file = directory.0.join("install.sh");
    let _ = writeln!(
        out,
        "Downloading the installer for {latest} and installing it in {} ...",
        Escaped(&prefix.display().to_string())
    );
    let downloaded = run_curl(context, &download_arguments(latest, &file), FETCH_LIMIT)?;
    if !downloaded.success {
        return Err(format!(
            "could not download the installer: {}. Nothing was changed.",
            text(&downloaded.stderr)
        ));
    }
    if !std::fs::read(&file).is_ok_and(|bytes| bytes.starts_with(b"#!/bin/sh\n")) {
        return Err(
            "the downloaded installer is not a shell script; nothing was changed".to_owned(),
        );
    }
    let installed = context
        .runner
        .run(
            Path::new(INSTALL_SH),
            &install_arguments(&file, latest, prefix),
            &inputs.child_env,
            INSTALL_LIMIT,
        )
        .map_err(|error| format!("could not run {INSTALL_SH}: {error}"))?;
    if installed.timed_out {
        return Err(format!(
            "the installer did not finish in {} minutes and was stopped. It restores the previous installation when it can; run dudes --version to see what is installed, then try again",
            INSTALL_LIMIT.as_secs() / 60
        ));
    }
    if !installed.success {
        let _ = err.write_all(&installed.stderr);
        return Err(
            "the installer failed. It restores the previous installation when it can; run dudes --version to see what is installed"
                .to_owned(),
        );
    }
    drop(directory);
    restart(context, latest, prefix, running, out, err)
}

fn restart(
    context: &Context<'_>,
    latest: &str,
    prefix: &Path,
    running: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let Context { inputs, daemon, .. } = *context;
    let bin = Escaped(&prefix.join("bin/dudes").display().to_string()).to_string();
    let summary = format!("{} → {latest}", inputs.current);
    if !running {
        let _ = writeln!(out, "{summary}");
        let _ = writeln!(out, "No daemon was running. Start it with: {bin} start");
        return Ok(exit::SUCCESS);
    }
    let by_hand = format!("Run {bin} stop, then {bin} start");
    let exported = daemon.export_sign_ins();
    daemon.stop(out, err).map_err(|reason| {
        format!("{latest} is installed, but the old daemon did not stop ({reason}). {by_hand}")
    })?;
    if !lifecycle::wait_for_unlock(&inputs.state_dir, inputs.lock_wait) {
        return Err(format!(
            "{latest} is installed, but a process still holds the lock in {} after the daemon stopped. {by_hand}",
            inputs.state_dir.display()
        ));
    }
    let version = daemon.start(err).map_err(|reason| {
        format!("{latest} is installed, but the daemon did not start ({reason}). Run {bin} start")
    })?;
    if version != latest {
        return Err(format!(
            "{latest} is installed, but the daemon that answered reports {version}. {by_hand}"
        ));
    }
    let _ = writeln!(out, "{summary}");
    if let Some(report) = sign_in_report(daemon, exported) {
        let _ = writeln!(out, "{report}");
    }
    Ok(exit::SUCCESS)
}

/// Hand the sign-ins that were exported before the stop to the daemon that now answers, and say
/// how many were kept. Any failure ends in the plain "sign in again": the upgrade itself already
/// succeeded.
fn sign_in_report(daemon: &dyn Restart, exported: Result<SessionExport, String>) -> Option<String> {
    let again = "Run dudes open to sign in again.";
    let Ok(exported) = exported else {
        return Some(format!(
            "This upgrade could not keep Floor sign-ins: the running version did not hand them over (a version from before this feature cannot). If a Floor tab was open, {}",
            again.to_lowercase()
        ));
    };
    let held = exported.sessions.len();
    if held == 0 {
        return None;
    }
    let kept = daemon.import_sign_ins(&exported).unwrap_or(0);
    Some(if kept == 0 {
        format!("Kept 0 of {held} Floor sign-ins. {again}")
    } else {
        format!("Kept {kept} of {held} Floor sign-ins. Reload an open Floor tab to continue.")
    })
}

/// The restart half of `dudes upgrade` against the daemon `daemon` reaches, for the tests that run
/// it against a real daemon. It installs nothing: the caller has already put the new files where
/// the daemon looks for them.
pub(crate) fn restart_only(
    daemon: &dyn Restart,
    state_dir: &Path,
    latest: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    struct NoRunner;
    impl Runner for NoRunner {
        fn run(
            &self,
            _: &Path,
            _: &[OsString],
            _: &[EnvVar],
            _: Duration,
        ) -> std::io::Result<Output> {
            Err(std::io::Error::other("the restart runs no installer"))
        }
    }
    let inputs = Inputs {
        windows: false,
        current: String::new(),
        executable: PathBuf::new(),
        temp_dir: PathBuf::new(),
        child_env: Vec::new(),
        state_dir: state_dir.to_path_buf(),
        lock_wait: Duration::from_secs(10),
        daemon_override: None,
    };
    let context = Context {
        inputs: &inputs,
        runner: &NoRunner,
        daemon,
    };
    restart(&context, latest, Path::new("/"), true, out, err)
}

#[cfg(test)]
mod tests;
