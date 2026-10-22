//! What `dudes quickstart` sets up. It asks nothing: [`defaults`] turns on the engines it finds and
//! the Floor's actions, `crate::args` parses the options that add to them into the same [`Plan`],
//! and nothing in this module writes a file, calls the daemon or starts a process. `crate::run`
//! writes the operator file once, starts the daemon or applies the change to the one that runs,
//! reports the engines, makes a hire when one was asked for and opens the Floor.

use std::io::{self, BufRead, Write};
use std::path::Path;

use dudes_proto::rpc::system::{ProviderProbe, SignIn};
use dudes_types::Provider;

use crate::config::{Change, ConfigError, Draft, Key, Settings};

/// The role a first hire gets unless the operator names one.
pub const DEFAULT_ROLE: &str = "engineer";
pub use dudes_config::check::{
    DEFAULT_DEPARTMENT, DEFAULT_HOP_CAP, DEFAULT_PREDICATE_TIMEOUT_S, NO_TEST_CHECK, changed_check,
};
/// The next step when the daemon has no enrolled project.
pub const NO_PROJECT_HINT: &str =
    "No project yet. In your project's folder run `dudes project add`.";

/// Everything quickstart sets up beyond starting the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Settings the options name, as `dudes config set` takes them.
    pub changes: Vec<Change>,
    /// The command the daemon runs to decide a goal is done, shared by the enrolled workspace and
    /// the first hire's goal.
    pub check: Option<Check>,
    /// Enroll the current directory for the Floor's Hire.
    pub enroll: Option<Enroll>,
    /// Hire a first agent in the current directory once the daemon runs.
    pub hire: Option<Hire>,
    /// A running daemon that reads other settings is stopped and started again, which ends any
    /// turn it is running.
    pub restart: bool,
}

/// A completion check: a typed argv the daemon resolves and runs itself, never a shell string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// Seconds before the daemon kills the check.
    pub timeout_s: u64,
    /// The most turns an agent takes on one goal.
    pub hop_cap: u32,
}

/// The current directory as one `[[floor.onboarding.workspaces]]` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enroll {
    /// How the Floor names the check.
    pub check_label: String,
    /// The department the Floor hires into.
    pub department: String,
    /// Dollars per day for each agent the Floor hires here, as typed.
    pub agent_usd_per_day: Option<String>,
    /// Dollars for each goal the Floor sets here, as typed.
    pub goal_usd: Option<String>,
}

/// The first agent, hired in the current directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hire {
    /// Its display name.
    pub name: String,
    /// Its role.
    pub role: String,
    /// The engine it runs on.
    pub provider: Provider,
    /// Its goal; `None` hires it idle, and an idle agent takes no turn.
    pub goal: Option<String>,
    /// A daily dollar ceiling, in whole dollars.
    pub usd_per_day: Option<u64>,
    /// A daily token ceiling.
    pub tokens_per_day: Option<u64>,
}

/// Why a prompt or a setting stopped.
#[derive(Debug, thiserror::Error)]
pub enum Stopped {
    /// The terminal's input ended.
    #[error("the terminal's input ended")]
    Ended,
    /// An answer the operator file cannot take.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Reading or writing the terminal failed.
    #[error("the terminal: {0}")]
    Io(#[from] io::Error),
}

/// The operator's terminal: a question is written out, and one line answers it.
pub struct Terminal<'a> {
    input: &'a mut dyn BufRead,
    out: &'a mut dyn Write,
}

impl<'a> Terminal<'a> {
    /// A terminal reading answers from `input` and writing questions to `out`.
    pub fn new(input: &'a mut dyn BufRead, out: &'a mut dyn Write) -> Self {
        Self { input, out }
    }

    /// Write one line.
    ///
    /// # Errors
    ///
    /// Whatever writing returns.
    pub fn say(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.out, "{text}")
    }

    fn answer(&mut self, question: &str) -> Result<String, Stopped> {
        write!(self.out, "{question} ")?;
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(self.out)?;
            return Err(Stopped::Ended);
        }
        Ok(line.trim().to_owned())
    }

    /// Ask a yes-or-no question; Enter takes `default`.
    ///
    /// # Errors
    ///
    /// [`Stopped::Ended`] when the input ends, or a terminal failure.
    pub fn yes(&mut self, question: &str, default: bool) -> Result<bool, Stopped> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match self
                .answer(&format!("{question} {hint}"))?
                .to_ascii_lowercase()
                .as_str()
            {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("Answer y or n.")?,
            }
        }
    }

    /// Ask until `parse` takes the answer; Enter offers `default`, or the empty answer without one.
    ///
    /// # Errors
    ///
    /// [`Stopped::Ended`] when the input ends, or a terminal failure.
    pub fn value<T>(
        &mut self,
        question: &str,
        default: Option<&str>,
        parse: impl Fn(&str) -> Result<T, String>,
    ) -> Result<T, Stopped> {
        let question = match default {
            Some(default) => format!("{question} [{default}]:"),
            None => format!("{question}:"),
        };
        loop {
            let answer = self.answer(&question)?;
            let text = match default {
                Some(default) if answer.is_empty() => default,
                _ => answer.as_str(),
            };
            match parse(text) {
                Ok(value) => return Ok(value),
                Err(why) => self.say(&why)?,
            }
        }
    }
}

/// The engines quickstart can turn on: those whose program is found on this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Host {
    /// Engines whose program is a runnable file on this process's `PATH`.
    pub found: Vec<Provider>,
}

impl Host {
    /// What `env` shows: the engines on its `PATH`.
    #[must_use]
    pub fn of(env: &crate::env::Environment) -> Self {
        let path = env
            .daemon_env()
            .iter()
            .find(|var| var.name() == "PATH")
            .map(|var| var.expose_value().to_owned());
        let found = [Provider::Claude, Provider::Codex, Provider::Pi]
            .into_iter()
            .filter(|provider| on_path(provider.as_str(), path.as_deref()))
            .collect();
        Self { found }
    }
}

/// Whether a runnable file called `name` sits in an absolute entry of `path`, as the daemon's own
/// search reads it. This only shapes what quickstart reports; the daemon decides what it launches.
fn on_path(name: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    path.is_some_and(|path| {
        std::env::split_paths(path)
            .filter(|entry| entry.is_absolute())
            .any(|entry| runnable(&entry.join(&file)))
    })
}

#[cfg(unix)]
fn runnable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(candidate: &Path) -> bool {
    candidate.is_file()
}

/// The Floor switches a file chooses among. The terminal is not one of them: a new install
/// leaves it off, and turning it on takes a restart.
const FLOOR_ACTIONS: [Key; 11] = [
    Key::Answer,
    Key::Hire,
    Key::Control,
    Key::Goal,
    Key::Retire,
    Key::Renew,
    Key::Recovery,
    Key::Board,
    Key::Project,
    Key::Git,
    Key::Diff,
];

/// The changed-file views, which stay off until the operator turns them on in the Floor.
const CHANGED_FILE_VIEWS: [Key; 2] = [Key::Git, Key::Diff];

/// Turn on the engines this machine has and, on a file that turns no Floor action on yet, the
/// Floor's actions, saying what it did. Nothing is asked.
///
/// # Errors
///
/// [`Stopped`] when the file refuses a change or `out` cannot be written.
pub fn defaults(
    out: &mut dyn Write,
    draft: &mut Draft,
    plan: &mut Plan,
    host: &Host,
) -> Result<(), Stopped> {
    engines_step(out, draft, plan, host)?;
    writeln!(out)?;
    if FLOOR_ACTIONS
        .iter()
        .any(|key| draft.settings().satisfies(*key))
    {
        writeln!(
            out,
            "Floor actions: your file already chooses which ones are on, so quickstart keeps that \
             choice."
        )?;
        return Ok(());
    }
    for key in FLOOR_ACTIONS
        .into_iter()
        .filter(|key| !CHANGED_FILE_VIEWS.contains(key))
    {
        set(draft, plan, key, true)?;
    }
    Ok(())
}

/// Claude Code is always on. Codex and Pi are turned on when their program is found and
/// otherwise left as the file has them.
fn engines_step(
    out: &mut dyn Write,
    draft: &mut Draft,
    plan: &mut Plan,
    host: &Host,
) -> Result<(), Stopped> {
    writeln!(out, "Engines.")?;
    let claude = if host.found.contains(&Provider::Claude) {
        "found"
    } else {
        "not found on your PATH; install Claude Code before an agent can run"
    };
    writeln!(out, "  claude: always on, {claude}")?;
    for (provider, key) in [(Provider::Codex, Key::Codex), (Provider::Pi, Key::Pi)] {
        let was_on = draft.settings().satisfies(key);
        let line = match (host.found.contains(&provider), was_on) {
            (true, true) => "found, already on",
            (true, false) => {
                set(draft, plan, key, true)?;
                "found, turned on"
            }
            (false, true) => "not found on your PATH, left on as your file has it",
            (false, false) => "not found on your PATH, left off",
        };
        writeln!(out, "  {provider}: {line}")?;
    }
    writeln!(
        out,
        "The daemon reports each engine's sign-in once it starts."
    )?;
    Ok(())
}

fn set(draft: &mut Draft, plan: &mut Plan, key: Key, on: bool) -> Result<(), Stopped> {
    let change = Change::set(key, if on { "on" } else { "off" })
        .expect("on and off are what a switch or an adapter table takes");
    draft.apply(&change)?;
    plan.changes.push(change);
    Ok(())
}

/// Whether the file turns `provider`'s engine on. Claude Code has no table and is always on.
#[must_use]
pub fn engine_on(file: &Settings, provider: Provider) -> bool {
    provider == Provider::Claude || file.adapters().contains(&provider)
}

/// Text the Floor shows: 1 to 128 bytes with no control character.
pub(crate) fn display_text(text: &str) -> Result<String, String> {
    if (1..=128).contains(&text.len()) && !text.chars().any(char::is_control) {
        Ok(text.to_owned())
    } else {
        Err("Use 1 to 128 characters.".to_owned())
    }
}

/// The engines step after the daemon starts: where the daemon's own search found each engine it
/// turned on, and what that engine's status command said about its sign-in.
///
/// # Errors
///
/// Whatever writing returns.
pub fn report_engines(out: &mut dyn Write, found: &[ProviderProbe]) -> io::Result<()> {
    if found.is_empty() {
        return writeln!(
            out,
            "The daemon has not finished looking for its engines; the Floor's Setup view shows \
             what it finds."
        );
    }
    writeln!(out, "Engines the daemon turned on:")?;
    for probe in found {
        let Some(path) = probe.command.as_ref().filter(|_| probe.ok) else {
            writeln!(out, "  {}: not found on the daemon's PATH", probe.provider)?;
            continue;
        };
        let sign_in = match (probe.provider, probe.sign_in) {
            (Provider::Pi, _) => format!("; {}", crate::login::PI_GUIDANCE),
            (_, Some(SignIn::SignedIn)) => ", signed in".to_owned(),
            (_, Some(SignIn::SignedOut)) => ", signed out".to_owned(),
            (_, Some(SignIn::Unknown)) => format!(
                ", sign-in unknown ({})",
                probe.detail.as_deref().unwrap_or("no answer")
            ),
            (_, None) => String::new(),
        };
        writeln!(
            out,
            "  {}: found at {}",
            probe.provider,
            dudes_types::Escaped(&format!("{}{sign_in}", path.display()))
        )?;
    }
    Ok(())
}

/// What a quickstart run changed, to print once the file is written.
#[derive(Debug)]
pub struct Summary {
    /// One line for each setting and each enrolled project.
    changes: String,
    /// The changed-file views that stay off, with where to turn them on.
    views: Option<String>,
}

impl Summary {
    /// The changes `plan` makes, enrolling at `cwd`, to the file whose settings are `settings`.
    #[must_use]
    pub fn of(plan: &Plan, settings: &Settings, cwd: Option<&Path>) -> Self {
        use std::fmt::Write as _;
        let mut changes = String::new();
        for change in &plan.changes {
            let _ = writeln!(changes, "  set {change}");
        }
        if let (Some(enroll), Some(check)) = (&plan.enroll, &plan.check) {
            let here = cwd.map_or_else(
                || "the current directory".to_owned(),
                |cwd| cwd.display().to_string(),
            );
            let _ = writeln!(
                changes,
                "  enroll {here} in {}, checked by `{}` (\"{}\"), at most {} turns a goal",
                enroll.department,
                check.argv.join(" "),
                enroll.check_label,
                check.hop_cap
            );
        }
        let off: Vec<String> = CHANGED_FILE_VIEWS
            .into_iter()
            .filter(|key| !settings.satisfies(*key))
            .map(|key| key.to_string())
            .collect();
        let views = (!off.is_empty()).then(|| {
            format!(
                "The changed-file views ({}) are off. Turn them on in the Floor's Settings.",
                off.join(", ")
            )
        });
        Self { changes, views }
    }

    /// Print what was written to `config`, or that it already held these settings.
    ///
    /// # Errors
    ///
    /// Whatever writing returns.
    pub fn print(&self, out: &mut dyn Write, config: &Path, wrote: bool) -> io::Result<()> {
        if wrote {
            writeln!(
                out,
                "\nWrote {}:\n{}",
                config.display(),
                self.changes.trim_end()
            )?;
        } else {
            writeln!(out, "\n{} already holds these settings.", config.display())?;
        }
        if let Some(views) = &self.views {
            writeln!(out, "{views}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NINE: [Key; 9] = [
        Key::Answer,
        Key::Hire,
        Key::Control,
        Key::Goal,
        Key::Retire,
        Key::Renew,
        Key::Recovery,
        Key::Board,
        Key::Project,
    ];

    fn host(found: &[Provider]) -> Host {
        Host {
            found: found.to_vec(),
        }
    }

    fn run_defaults(original: Option<&str>, host: &Host) -> (Draft, Plan, String) {
        let home = tempfile::tempdir().expect("a home");
        let config = home.path().join("config.toml");
        if let Some(original) = original {
            std::fs::write(&config, original).expect("a file");
        }
        let mut draft = Draft::open(&config).expect("the file opens");
        let mut plan = Plan::default();
        let mut out = Vec::new();
        defaults(&mut out, &mut draft, &mut plan, host).expect("the defaults apply");
        (draft, plan, String::from_utf8(out).expect("text"))
    }

    /// A new file gets the nine Floor actions that a first run needs, in an order the file's own
    /// rules accept, and neither changed-file view nor the terminal.
    #[test]
    fn a_new_file_turns_on_nine_floor_actions() {
        let (draft, plan, _) = run_defaults(None, &host(&[Provider::Claude]));
        let wanted: Vec<Change> = NINE
            .iter()
            .map(|key| Change::set(*key, "on").expect("a switch"))
            .collect();
        assert_eq!(plan.changes, wanted);
        for key in [Key::Git, Key::Diff, Key::Terminal] {
            assert!(!draft.settings().satisfies(key), "{key} is on");
        }
        assert!(draft.settings().problems().is_empty());
    }

    /// A file that already turns any Floor action on keeps every Floor value and says so.
    #[test]
    fn a_file_with_one_floor_action_on_is_kept() {
        let (draft, plan, shown) = run_defaults(
            Some("[floor.git]\nenabled = true\n"),
            &host(&[Provider::Claude]),
        );
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
        assert!(shown.contains("keeps that choice."), "{shown}");
        assert!(draft.settings().satisfies(Key::Git));
        assert!(!draft.settings().satisfies(Key::Hire));
    }

    /// Codex and Pi are turned on when found, left as the file has them otherwise.
    #[test]
    fn engines_are_detected_not_asked() {
        let found = host(&[Provider::Claude, Provider::Codex, Provider::Pi]);
        let (_, plan, shown) = run_defaults(None, &found);
        assert!(plan.changes.starts_with(&[
            Change::set(Key::Codex, "on").expect("on"),
            Change::set(Key::Pi, "on").expect("on"),
        ]));
        assert!(shown.contains("codex: found, turned on"), "{shown}");
        assert!(shown.contains("pi: found, turned on"), "{shown}");

        let (draft, _, shown) = run_defaults(Some("[providers.codex]\nstreaming = true\n"), &found);
        assert!(draft.settings().satisfies(Key::Codex));
        assert!(shown.contains("codex: found, already on"), "{shown}");

        let (_, _, shown) = run_defaults(None, &host(&[Provider::Claude]));
        assert!(
            shown.contains("codex: not found on your PATH, left off"),
            "{shown}"
        );
    }

    /// The summary names each change and says which changed-file views stay off.
    #[test]
    fn the_summary_names_the_views_that_stay_off() {
        let (draft, plan, _) = run_defaults(None, &host(&[Provider::Claude]));
        let mut out = Vec::new();
        Summary::of(&plan, draft.settings(), None)
            .print(&mut out, Path::new("/c/config.toml"), true)
            .expect("a Vec takes every write");
        let text = String::from_utf8(out).expect("text");
        assert!(
            text.contains("Wrote /c/config.toml:\n  set floor.answer on\n"),
            "{text}"
        );
        assert!(
            text.contains("The changed-file views (floor.git, floor.diff) are off."),
            "{text}"
        );

        let (draft, plan, _) = run_defaults(
            Some("[floor.git]\nenabled = true\n[floor.diff]\nenabled = true\n"),
            &host(&[Provider::Claude]),
        );
        let mut out = Vec::new();
        Summary::of(&plan, draft.settings(), None)
            .print(&mut out, Path::new("/c/config.toml"), false)
            .expect("a Vec takes every write");
        let text = String::from_utf8(out).expect("text");
        assert!(text.contains("already holds these settings"), "{text}");
        assert!(!text.contains("changed-file views"), "{text}");
    }

    /// Each engine the daemon turned on gets its place and the daemon's sign-in answer; Pi gets
    /// its own way to sign in.
    #[test]
    fn the_engines_report_shows_where_each_engine_is_and_its_sign_in() {
        let probe =
            |provider, command: Option<&str>, sign_in, detail: Option<&str>| ProviderProbe {
                login_home: None,
                provider,
                command: command.map(std::path::PathBuf::from),
                version: None,
                ok: command.is_some(),
                detail: detail.map(str::to_owned),
                sign_in,
            };
        let mut out = Vec::new();
        report_engines(
            &mut out,
            &[
                probe(
                    Provider::Claude,
                    Some("/bin/claude"),
                    Some(SignIn::SignedIn),
                    None,
                ),
                probe(
                    Provider::Codex,
                    Some("/bin/codex"),
                    Some(SignIn::SignedOut),
                    None,
                ),
                probe(
                    Provider::Claude,
                    Some("/opt/claude"),
                    Some(SignIn::Unknown),
                    Some("a turn is running"),
                ),
                probe(Provider::Codex, None, None, None),
                probe(
                    Provider::Pi,
                    Some("/bin/pi"),
                    Some(SignIn::Unknown),
                    Some("Pi has no status command that is safe to run"),
                ),
                probe(Provider::Claude, Some("/old/claude"), None, None),
            ],
        )
        .expect("a Vec takes every write");

        assert_eq!(
            String::from_utf8(out).expect("text"),
            "Engines the daemon turned on:\n\
             \x20 claude: found at /bin/claude, signed in\n\
             \x20 codex: found at /bin/codex, signed out\n\
             \x20 claude: found at /opt/claude, sign-in unknown (a turn is running)\n\
             \x20 codex: not found on the daemon's PATH\n\
             \x20 pi: found at /bin/pi; Pi signs in inside its own session: run pi, then type \
             /login.\n\
             \x20 claude: found at /old/claude\n"
        );
    }

    #[test]
    fn the_engines_report_escapes_the_daemons_path_and_sign_in_detail() {
        let mut out = Vec::new();
        report_engines(
            &mut out,
            &[ProviderProbe {
                login_home: None,
                provider: Provider::Codex,
                command: Some("/bin/\u{1b}]52;c;aGk=\u{7}\ncodex: found\u{202e}".into()),
                version: None,
                ok: true,
                detail: Some("token \u{1b}[2J\nexpired\u{2066}".to_owned()),
                sign_in: Some(SignIn::Unknown),
            }],
        )
        .expect("a Vec takes every write");
        assert_eq!(
            String::from_utf8(out).expect("text"),
            "Engines the daemon turned on:\n\
             \x20 codex: found at /bin/\\u{1b}]52;c;aGk=\\u{7}\\u{a}codex: found\\u{202e}, \
             sign-in unknown (token \\u{1b}[2J\\u{a}expired\\u{2066})\n"
        );
    }
}
