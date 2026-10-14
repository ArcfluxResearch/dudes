//! `dudes upgrade`: find the latest release, and install it in place with the release's own
//! installer.
//!
//! This is the only place a Dudes program connects to GitHub, and only when the operator runs it.
//! The connection is made by the system `curl` at an absolute path, never by this binary, and the
//! installer's checksum check and restore logic do the rest. Nothing checks for updates on its own.
//!
//! The processes and the daemon are reached through [`Runner`] and [`Daemon`], so everything here
//! is drivable without a network or a daemon.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::args::UpgradeOptions;
use crate::exit;
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
const NOTICES: [&str; 3] = ["LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY.md"];
const PROGRAMS: [&str; 3] = ["duded", "dudes", "dudes-hook"];

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

/// Run a program and capture what it printed.
pub(crate) trait Runner {
    /// Whether `program` is a file.
    fn exists(&self, program: &Path) -> bool;
    fn run(
        &self,
        program: &Path,
        args: &[OsString],
        env: &[(OsString, OsString)],
    ) -> std::io::Result<Output>;
}

/// What `upgrade` needs from a daemon that may be running.
pub(crate) trait Daemon {
    /// Whether a daemon answers.
    fn running(&self) -> bool;
    /// The agents that are mid-turn, each as a printable `name (id)`.
    fn mid_turn(&self) -> Result<Vec<String>, String>;
    /// Stop it the way `dudes stop` does, or at once when `now`.
    fn stop(&self, now: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String>;
    /// Start it the way `dudes start` does, and return the version that answers.
    fn start(&self, err: &mut dyn Write) -> Result<String, String>;
}

/// Where this copy lives and what it may use.
pub(crate) struct Inputs {
    pub(crate) windows: bool,
    pub(crate) current: String,
    pub(crate) executable: PathBuf,
    pub(crate) temp_dir: PathBuf,
    pub(crate) child_env: Vec<(OsString, OsString)>,
}

fn arguments(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// The arguments that read where `releases/latest` redirects, without following it.
pub(crate) fn latest_arguments() -> Vec<OsString> {
    let url = format!("{REPOSITORY}/releases/latest");
    arguments(&[
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

/// The prefix `install.sh` installed this copy into, or why this copy is not one of its installs.
pub(crate) fn installed_prefix(executable: &Path) -> Result<PathBuf, String> {
    let bin = executable
        .parent()
        .filter(|bin| bin.file_name() == Some("bin".as_ref()));
    let prefix = bin.and_then(Path::parent);
    let installed = match (bin, prefix) {
        (Some(bin), Some(prefix)) => {
            PROGRAMS.iter().all(|name| bin.join(name).is_file())
                && NOTICES
                    .iter()
                    .all(|name| prefix.join("share/dudes").join(name).is_file())
        }
        _ => false,
    };
    match prefix {
        Some(prefix) if installed => Ok(prefix.to_path_buf()),
        _ => Err(format!(
            "this copy of dudes ({}) was not put there by install.sh, so dudes upgrade will not replace it. \
             If you built it from source, rebuild it: {REBUILD}",
            executable.display()
        )),
    }
}

/// A fresh directory only this user can enter, removed when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create(base: &Path) -> std::io::Result<Self> {
        use std::fs::DirBuilder;
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        for _ in 0..8 {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).map_err(std::io::Error::other)?;
            let path = base.join(format!("dudes-upgrade-{:016x}", u64::from_le_bytes(random)));
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
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

fn failed(err: &mut dyn Write, message: &str) -> u8 {
    let _ = render::local_error(err, message);
    exit::OTHER
}

fn latest_release(inputs: &Inputs, runner: &dyn Runner) -> Result<Version, String> {
    let output = runner
        .run(Path::new(CURL), &latest_arguments(), &inputs.child_env)
        .map_err(|error| format!("could not run {CURL}: {error}"))?;
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
    options: UpgradeOptions,
    runner: &'a dyn Runner,
    daemon: &'a dyn Daemon,
}

/// Run `dudes upgrade`. Returns the process's exit code.
pub(crate) fn run(
    inputs: &Inputs,
    options: UpgradeOptions,
    runner: &dyn Runner,
    daemon: &dyn Daemon,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    if inputs.windows {
        return failed(
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
    if !runner.exists(Path::new(CURL)) {
        return failed(
            err,
            &format!(
                "{CURL} was not found. Run the installer yourself, then dudes stop and dudes start: {ONE_LINER}"
            ),
        );
    }
    let latest = match latest_release(inputs, runner) {
        Ok(latest) => latest,
        Err(message) => return failed(err, &message),
    };
    let latest_text = latest.text.clone();
    let _ = writeln!(out, "Current version: {}", Escaped(&inputs.current));
    let _ = writeln!(out, "Latest release:  {latest_text}");
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
    let context = Context {
        inputs,
        options,
        runner,
        daemon,
    };
    match install(&context, &latest_text, out, err) {
        Ok(code) => code,
        Err(message) => failed(err, &message),
    }
}

fn install(
    context: &Context<'_>,
    latest: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let Context {
        inputs,
        options,
        runner,
        daemon,
    } = *context;
    let prefix = installed_prefix(&inputs.executable)?;
    let running = daemon.running();
    if running && !options.now {
        let busy = daemon.mid_turn().map_err(|reason| {
            format!("could not list the agents that are mid-turn ({reason}); nothing was changed. Pass --now to upgrade anyway")
        })?;
        if !busy.is_empty() {
            return Err(format!(
                "these agents are mid-turn: {}. Restarting the daemon would end their turns. Wait for them to finish, or pass --now. Nothing was changed.",
                busy.join(", ")
            ));
        }
    }
    let directory = PrivateDir::create(&inputs.temp_dir)
        .map_err(|error| format!("could not create a private temporary directory: {error}"))?;
    let file = directory.0.join("install.sh");
    let _ = writeln!(
        out,
        "Downloading the installer for {latest} and installing it in {} ...",
        Escaped(&prefix.display().to_string())
    );
    let downloaded = runner
        .run(
            Path::new(CURL),
            &download_arguments(latest, &file),
            &inputs.child_env,
        )
        .map_err(|error| format!("could not run {CURL}: {error}"))?;
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
    let installed = runner
        .run(
            Path::new(INSTALL_SH),
            &install_arguments(&file, latest, &prefix),
            &inputs.child_env,
        )
        .map_err(|error| format!("could not run {INSTALL_SH}: {error}"))?;
    if !installed.success {
        let _ = err.write_all(&installed.stderr);
        return Err(
            "the installer failed. It restores the previous installation when it can; run dudes --version to see what is installed"
                .to_owned(),
        );
    }
    drop(directory);
    restart(context, latest, &prefix, running, out, err)
}

fn restart(
    context: &Context<'_>,
    latest: &str,
    prefix: &Path,
    running: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let Context {
        inputs,
        options,
        daemon,
        ..
    } = *context;
    let bin = prefix.join("bin/dudes");
    let bin = Escaped(&bin.display().to_string()).to_string();
    let summary = format!("{} → {latest}", inputs.current);
    if !running {
        let _ = writeln!(out, "{summary}");
        let _ = writeln!(out, "No daemon was running. Start it with: {bin} start");
        return Ok(exit::SUCCESS);
    }
    daemon.stop(options.now, out, err).map_err(|reason| {
        format!("{latest} is installed, but the old daemon did not stop ({reason}). Run {bin} stop, then {bin} start")
    })?;
    let version = daemon.start(err).map_err(|reason| {
        format!("{latest} is installed, but the daemon did not start ({reason}). Run {bin} start")
    })?;
    let _ = writeln!(out, "{summary}");
    if version != latest {
        let _ = render::warning(
            err,
            &format!("the daemon that answered reports {version}, not {latest}"),
        );
    }
    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests;
