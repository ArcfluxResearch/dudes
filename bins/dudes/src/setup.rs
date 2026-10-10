//! What `dudes quickstart` sets up: the operator's choices, asked on a terminal or passed as flags.
//!
//! [`guided`] asks them one question at a time; `crate::args` parses the same choices from flags
//! into the same [`Plan`]. Nothing in this module writes a file, calls the daemon or starts a
//! process: `crate::run` writes the operator file once, starts or restarts the daemon, reports the
//! engines, prints the Floor link, makes the hire and offers a login.

use std::io::{self, BufRead, Write};
use std::path::Path;

use dudes_proto::rpc::system::{ProviderProbe, SignIn};
use dudes_types::Provider;

use crate::config::{Change, ConfigError, Draft, Key, Settings};

/// The department an enrolled workspace and a first hire get unless the operator names one.
pub const DEFAULT_DEPARTMENT: &str = "engineering";
/// The role a first hire gets unless the operator names one.
pub const DEFAULT_ROLE: &str = "engineer";
/// The most turns an agent takes on one goal unless the operator says otherwise.
pub const DEFAULT_HOP_CAP: u32 = 5;
/// The name the guided first hire offers.
const DEFAULT_NAME: &str = "Kim";

/// Everything quickstart sets up beyond starting the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Engine, Floor-action and budget settings, as `dudes config set` takes them.
    pub changes: Vec<Change>,
    /// The command the daemon runs to decide a goal is done, shared by the enrolled workspace and
    /// the first hire's goal.
    pub check: Option<Check>,
    /// Enroll the current directory for the Floor's Hire.
    pub enroll: Option<Enroll>,
    /// Hire a first agent in the current directory once the daemon runs.
    pub hire: Option<Hire>,
    /// The operator agreed that a running daemon with other settings is stopped and started
    /// again, which ends any turn it is running.
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

/// Why the guided setup stopped before it wrote anything.
#[derive(Debug, thiserror::Error)]
pub enum Stopped {
    /// The terminal's input ended.
    #[error("quickstart stopped at the end of its input; nothing was written")]
    Ended,
    /// The operator said no to the summary.
    #[error("quickstart stopped; nothing was written")]
    Declined,
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

/// What this machine already tells quickstart, so it asks only what it cannot work out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Host {
    /// Engines whose program is a runnable file on this process's `PATH`.
    pub found: Vec<Provider>,
    /// Engines for which this process holds a credential that bills each token.
    pub billed: Vec<Provider>,
}

impl Host {
    /// What `env` shows: the engines on its `PATH` and the credentials it holds.
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
        Self {
            found,
            billed: env.billed().to_vec(),
        }
    }
}

/// Whether a runnable file called `name` sits in an absolute entry of `path`, as the daemon's own
/// search reads it. This only shapes the questions; the daemon decides what it launches.
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

/// The Floor switches the single Floor question turns on and off together.
const FLOOR_ACTIONS: [Key; 6] = [
    Key::Answer,
    Key::Hire,
    Key::Control,
    Key::Goal,
    Key::Retire,
    Key::Terminal,
];

/// Ask for every choice, applying each setting to `draft` as it is answered.
///
/// `cwd` is the canonical current directory, offered for enrollment when it holds `.git`.
///
/// # Errors
///
/// [`Stopped`] when the input ends, the terminal fails or the file refuses an answer.
pub fn guided(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    config: &Path,
    cwd: Option<&Path>,
    host: &Host,
) -> Result<Plan, Stopped> {
    let mut plan = Plan::default();
    term.say(&format!(
        "Dudes quickstart sets up {}, starts the daemon and prints a link to the Floor. Enter \
         takes the answer in capitals or in brackets.",
        config.display()
    ))?;
    engines_step(term, draft, &mut plan, host)?;
    floor_step(term, draft, &mut plan)?;
    cap_step(term, draft, &mut plan, host)?;
    if let Some(cwd) = cwd.filter(|cwd| cwd.join(".git").exists()) {
        workspace_step(term, draft.settings(), cwd, &mut plan)?;
    }
    hire_step(term, draft.settings(), cwd, host, &mut plan)?;
    Ok(plan)
}

/// Claude Code is always on. Codex and Pi are turned on when their program is found and
/// otherwise left as the file has them. Nothing here is asked.
fn engines_step(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    plan: &mut Plan,
    host: &Host,
) -> Result<(), Stopped> {
    term.say("\nEngines.")?;
    let claude = if host.found.contains(&Provider::Claude) {
        "found"
    } else {
        "not found on your PATH; install Claude Code before an agent can run"
    };
    term.say(&format!("  claude: always on, {claude}"))?;
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
        term.say(&format!("  {provider}: {line}"))?;
    }
    term.say("The daemon reports each engine's sign-in once it starts.")?;
    Ok(())
}

/// One question for the Floor's actions, or one line when the file already holds its own mix.
fn floor_step(term: &mut Terminal<'_>, draft: &mut Draft, plan: &mut Plan) -> Result<(), Stopped> {
    term.say("")?;
    let settings = draft.settings();
    let on = FLOOR_ACTIONS
        .iter()
        .filter(|key| settings.satisfies(**key))
        .count();
    if on != 0 && on != FLOOR_ACTIONS.len() {
        return Ok(term.say(
            "Floor actions: your file already chooses which ones are on, so quickstart keeps that \
             choice.",
        )?);
    }
    let allow = term.yes(
        "Let the Floor act on your agents: hire, pause, answer, change goals, retire and attach a \
         terminal? Anyone holding your one-time Floor link could start agents that act with your \
         account's full reach.",
        true,
    )?;
    for key in FLOOR_ACTIONS {
        if allow != draft.settings().satisfies(key) {
            set(draft, plan, key, allow)?;
        }
    }
    if !allow && draft.settings().satisfies(Key::Renew) {
        set(draft, plan, Key::Renew, false)?;
    }
    Ok(())
}

/// The floor's daily cap, asked only when this process holds an API key for an engine that is on.
fn cap_step(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    plan: &mut Plan,
    host: &Host,
) -> Result<(), Stopped> {
    if draft.settings().satisfies(Key::FloorUsdPerDay) {
        return Ok(term.say(&format!(
            "\nThe floor's daily cap stays at ${}.",
            draft.settings().shown(Key::FloorUsdPerDay)
        ))?);
    }
    let billed = billed_engines(draft.settings(), host);
    if billed.is_empty() {
        return Ok(());
    }
    term.say(&format!(
        "\nSpending cap. This shell holds an API key for {}, which bills for each token.",
        billed.join(" and "),
    ))?;
    let change = term.value(
        "Daily dollar cap for the whole floor (Enter for none)",
        None,
        |text| {
            if text.is_empty() {
                Ok(None)
            } else {
                floor_cap(text).map(Some)
            }
        },
    )?;
    if let Some(change) = change {
        draft.apply(&change)?;
        plan.changes.push(change);
    }
    Ok(())
}

/// The engines that are on and for which this process holds a credential that bills each token.
fn billed_engines(file: &Settings, host: &Host) -> Vec<&'static str> {
    host.billed
        .iter()
        .filter(|provider| engine_on(file, **provider))
        .map(|provider| provider.as_str())
        .collect()
}

/// The check a repository suggests for itself: `None` when nothing in it names one.
#[must_use]
pub fn suggest_check(dir: &Path) -> Option<Vec<String>> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let argv: &[&str] = if dir.join("Cargo.toml").is_file() {
        &["cargo", "test"]
    } else if read("package.json").is_some_and(|text| npm_test(&text)) {
        &["npm", "test"]
    } else if dir.join("go.mod").is_file() {
        &["go", "test", "./..."]
    } else if dir.join("pyproject.toml").is_file() || dir.join("pytest.ini").is_file() {
        &["pytest"]
    } else if ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .filter_map(|name| read(name))
        .any(|text| text.lines().any(|line| line.starts_with("test:")))
    {
        &["make", "test"]
    } else {
        return None;
    };
    Some(argv.iter().map(|word| (*word).to_owned()).collect())
}

/// Whether `package.json` has a `test` script, other than the placeholder `npm init` writes.
fn npm_test(manifest: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(manifest)
        .ok()
        .and_then(|json| json["scripts"]["test"].as_str().map(str::to_owned))
        .is_some_and(|script| !script.contains("no test specified"))
}

/// Ask for the completion check, offering `suggestion`. `-` skips when `skippable`, and so does
/// Enter when there is nothing to offer.
fn ask_check(
    term: &mut Terminal<'_>,
    question: &str,
    suggestion: Option<&[String]>,
    skippable: bool,
) -> Result<Option<Vec<String>>, Stopped> {
    let suggested = suggestion.map(|argv| argv.join(" "));
    let prompt = match (&suggested, skippable) {
        (Some(text), true) => format!("{question} [{text}] (- to skip):"),
        (Some(text), false) => format!("{question} [{text}]:"),
        (None, true) => format!("{question} (- to skip):"),
        (None, false) => format!("{question}:"),
    };
    loop {
        let answer = term.answer(&prompt)?;
        let text = match (answer.as_str(), &suggested) {
            ("-", _) | ("", None) if skippable => return Ok(None),
            ("", Some(text)) => text.as_str(),
            ("-", _) => "",
            (text, _) => text,
        };
        match words(text) {
            Ok(Some(argv)) => return Ok(Some(argv)),
            Ok(None) => term.say("Type the program and its arguments.")?,
            Err(why) => term.say(&why)?,
        }
    }
}

fn workspace_step(
    term: &mut Terminal<'_>,
    file: &Settings,
    cwd: &Path,
    plan: &mut Plan,
) -> Result<(), Stopped> {
    if let Some(id) = file.enrolled_at(cwd) {
        return Ok(term.say(&format!(
            "\nWorkspace. {} is enrolled as {id}.",
            cwd.display()
        ))?);
    }
    let hires = if file.satisfies(Key::Hire) {
        "the Floor can hire agents into it"
    } else {
        "the Floor can hire agents into it once floor.hire is on"
    };
    term.say(&format!(
        "\nWorkspace. {} is a git repository. Enrolled, {hires}. A completion check is a command \
         the daemon runs itself to decide that a goal is done, typed as a program and its \
         arguments separated by spaces.",
        cwd.display()
    ))?;
    let suggestion = suggest_check(cwd);
    let Some(argv) = ask_check(
        term,
        "Completion check for this repository",
        suggestion.as_deref(),
        true,
    )?
    else {
        return Ok(());
    };
    term.say(
        "A goal normally needs a check that fails until that goal is met. Add a check for each \
         kind of goal with dudes workspace check add.",
    )?;
    let label = argv.join(" ");
    plan.enroll = Some(Enroll {
        check_label: display_text(&label)
            .map_err(|why| Stopped::Io(io::Error::new(io::ErrorKind::InvalidInput, why)))?,
        department: DEFAULT_DEPARTMENT.to_owned(),
        agent_usd_per_day: None,
        goal_usd: None,
    });
    plan.check = Some(check(argv));
    Ok(())
}

fn hire_step(
    term: &mut Terminal<'_>,
    file: &Settings,
    cwd: Option<&Path>,
    host: &Host,
    plan: &mut Plan,
) -> Result<(), Stopped> {
    term.say("")?;
    let runnable = [Provider::Claude, Provider::Codex, Provider::Pi]
        .into_iter()
        .any(|provider| engine_on(file, provider) && host.found.contains(&provider));
    if !term.yes("Hire a first agent?", runnable)? {
        return Ok(());
    }
    let name = term.value("Name", Some(DEFAULT_NAME), display_text)?;
    let engines: Vec<Provider> = [Provider::Claude, Provider::Codex, Provider::Pi]
        .into_iter()
        .filter(|provider| engine_on(file, *provider))
        .collect();
    let provider = if engines.len() == 1 {
        Provider::Claude
    } else {
        let names: Vec<&str> = engines.iter().map(|provider| provider.as_str()).collect();
        term.value(
            &format!("Engine ({})", names.join(", ")),
            Some("claude"),
            |text| {
                engines
                    .iter()
                    .copied()
                    .find(|provider| provider.as_str() == text)
                    .ok_or_else(|| format!("Choose one of {}.", names.join(", ")))
            },
        )?
    };
    if provider == Provider::Codex {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        term.say("Codex's command sandbox permits writes in this workspace, its permitted temporary directories and its own outgoing mailbox. The completion check runs this workspace's code with no sandbox.")?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        term.say("Codex's command sandbox permits writes in this workspace and its permitted temporary directories. Mail is unavailable on this platform. The completion check runs this workspace's code with no sandbox.")?;
    }
    let goal = term.value("Goal (Enter for none)", None, |text| {
        Ok((!text.is_empty()).then(|| text.to_owned()))
    })?;
    if goal.is_some() && plan.check.is_none() {
        term.say(
            "The goal needs a completion check: a command the daemon runs itself to decide that \
             it is done, typed as a program and its arguments separated by spaces.",
        )?;
        let suggestion = cwd.and_then(suggest_check);
        let argv = ask_check(term, "Completion check", suggestion.as_deref(), false)?
            .expect("a check that cannot be skipped is an answer");
        plan.check = Some(check(argv));
    }
    let (mut usd_per_day, mut tokens_per_day) = (None, None);
    if host.billed.contains(&provider) {
        term.say(&format!(
            "{provider} bills an API key for each token. A ceiling for this agent is optional."
        ))?;
        usd_per_day = term.value("Whole dollars per day (Enter for none)", None, optional)?;
        tokens_per_day = term.value("Tokens per day (Enter for none)", None, optional)?;
    }
    plan.hire = Some(Hire {
        name,
        role: DEFAULT_ROLE.to_owned(),
        provider,
        goal,
        usd_per_day,
        tokens_per_day,
    });
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

fn check(argv: Vec<String>) -> Check {
    Check {
        argv,
        timeout_s: crate::args::DEFAULT_PREDICATE_TIMEOUT_S,
        hop_cap: DEFAULT_HOP_CAP,
    }
}

/// A program and its arguments, split at spaces; `None` for an empty answer. Quotes are refused
/// rather than kept inside an argument: a space inside one cannot be typed here, and
/// `-- <program> [args...]` on the command line carries it.
fn words(text: &str) -> Result<Option<Vec<String>>, String> {
    if text.contains(['"', '\'', '\\']) {
        return Err(
            "Quotes are not read here, and each space starts a new argument. For an \
                    argument with a space, pass the check after `--` to `dudes quickstart`."
                .to_owned(),
        );
    }
    let argv: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    Ok((!argv.is_empty()).then_some(argv))
}

/// Text the Floor shows: 1 to 128 bytes with no control character.
pub(crate) fn display_text(text: &str) -> Result<String, String> {
    if (1..=128).contains(&text.len()) && !text.chars().any(char::is_control) {
        Ok(text.to_owned())
    } else {
        Err("Use 1 to 128 characters.".to_owned())
    }
}

/// A floor-wide daily dollar cap above zero, as the change that sets it.
fn floor_cap(text: &str) -> Result<Change, String> {
    crate::args::positive_dollars(text)
        .then(|| Change::set(Key::FloorUsdPerDay, text).ok())
        .flatten()
        .ok_or_else(|| "Type a number above zero, with at most six decimal places.".to_owned())
}

fn optional(text: &str) -> Result<Option<u64>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
        .map(Some)
        .ok_or_else(|| "Type a whole number above zero, or press Enter for none.".to_owned())
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

/// The summary the operator confirms before anything is written.
///
/// # Errors
///
/// Whatever writing returns.
pub fn summary(term: &mut Terminal<'_>, plan: &Plan, cwd: Option<&Path>) -> io::Result<()> {
    let out = &mut *term.out;
    writeln!(out, "\nQuickstart will:")?;
    for change in &plan.changes {
        writeln!(out, "  set {change}")?;
    }
    let here = cwd.map_or_else(
        || "the current directory".to_owned(),
        |cwd| cwd.display().to_string(),
    );
    if let (Some(enroll), Some(check)) = (&plan.enroll, &plan.check) {
        writeln!(
            out,
            "  enroll {here} in {}, checked by `{}` (\"{}\"), at most {} turns a goal",
            enroll.department,
            check.argv.join(" "),
            enroll.check_label,
            check.hop_cap
        )?;
    }
    writeln!(
        out,
        "  start the daemon, or restart it when it runs other settings"
    )?;
    if let Some(hire) = &plan.hire {
        let goal = hire.goal.as_deref().map_or_else(
            || "no goal".to_owned(),
            |goal| format!("the goal \"{goal}\""),
        );
        writeln!(
            out,
            "  hire {} on {} in {here}, with {goal}",
            hire.name, hire.provider
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIONS: [Key; 6] = FLOOR_ACTIONS;

    fn host(found: &[Provider], billed: &[Provider]) -> Host {
        Host {
            found: found.to_vec(),
            billed: billed.to_vec(),
        }
    }

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("a repository");
        std::fs::create_dir(repo.path().join(".git")).expect("a .git directory");
        for (name, text) in files {
            std::fs::write(repo.path().join(name), text).expect("a file");
        }
        repo
    }

    struct Run {
        plan: Result<Plan, Stopped>,
        draft: Draft,
        shown: String,
        asked: usize,
    }

    /// Run [`guided`] over `original` as the operator file, answering one line per question, and
    /// check that the `script` questions were asked in that order.
    fn run_guided(original: &str, script: &[(&str, &str)], cwd: Option<&Path>, host: &Host) -> Run {
        let home = tempfile::tempdir().expect("a home");
        let config = home.path().join("config.toml");
        std::fs::write(&config, original).expect("a file");
        let mut draft = Draft::open(&config).expect("the file opens");
        let answers: Vec<&str> = script.iter().map(|(_, answer)| *answer).collect();
        let input = answers.join("\n") + "\n";
        let mut reader = io::Cursor::new(input.clone());
        let mut out = Vec::new();
        let plan = guided(
            &mut Terminal::new(&mut reader, &mut out),
            &mut draft,
            &config,
            cwd,
            host,
        );
        let shown = String::from_utf8(out).expect("the questions are text");
        let mut rest = shown.as_str();
        for (question, _) in script {
            let at = rest
                .find(question)
                .unwrap_or_else(|| panic!("`{question}` was not asked next in:\n{shown}"));
            rest = &rest[at + question.len()..];
        }
        Run {
            plan,
            draft,
            shown,
            asked: input[..usize::try_from(reader.position()).expect("small")]
                .matches('\n')
                .count(),
        }
    }

    fn change(key: Key, text: &str) -> Change {
        Change::set(key, text).expect("a value the key takes")
    }

    const FLOOR: &str = "Let the Floor act on your agents: hire, pause, answer, change goals, retire and attach a terminal? Anyone holding your one-time Floor link could start agents that act with your account's full reach. [Y/n]";

    /// Enter at every question on a fresh file asks at most five questions and leaves a Floor
    /// that can act, a repository enrolled with its suggested check, and one agent hired.
    #[test]
    fn enter_at_every_question_asks_at_most_five_and_turns_the_floor_on() {
        let repo = repo(&[("Cargo.toml", "[package]\n")]);
        let script = [
            (FLOOR, ""),
            (
                "Completion check for this repository [cargo test] (- to skip):",
                "",
            ),
            ("Hire a first agent? [Y/n]", ""),
            ("Name [Kim]:", ""),
            ("Goal (Enter for none):", ""),
        ];
        let run = run_guided(
            "",
            &script,
            Some(repo.path()),
            &host(&[Provider::Claude], &[]),
        );
        let shown = run.shown;
        let plan = run.plan.expect("Enter answers every question");
        assert_eq!(run.asked, 5, "{shown}");
        for key in ACTIONS {
            assert!(
                run.draft.settings().satisfies(key),
                "{key} is off:\n{shown}"
            );
        }
        assert!(!run.draft.settings().satisfies(Key::Renew), "{shown}");
        assert_eq!(
            plan.check.as_ref().map(|check| check.argv.clone()),
            Some(vec!["cargo".to_owned(), "test".to_owned()])
        );
        assert_eq!(
            plan.enroll
                .as_ref()
                .map(|enroll| enroll.check_label.as_str()),
            Some("cargo test")
        );
        assert_eq!(
            plan.hire.as_ref().map(|hire| hire.name.as_str()),
            Some("Kim")
        );
        assert_eq!(
            plan.hire.as_ref().map(|hire| hire.provider),
            Some(Provider::Claude)
        );
    }

    /// Codex and Pi are turned on when found and never asked about, and Codex streaming is never
    /// offered or changed.
    #[test]
    fn engines_are_detected_not_asked() {
        let found = host(&[Provider::Claude, Provider::Codex, Provider::Pi], &[]);
        let run = run_guided(
            "",
            &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &found,
        );
        let shown = run.shown;
        let plan = run.plan.expect("answered");
        assert_eq!(
            plan.changes,
            [change(Key::Codex, "on"), change(Key::Pi, "on")]
        );
        assert!(shown.contains("codex: found, turned on"), "{shown}");
        assert!(shown.contains("pi: found, turned on"), "{shown}");
        assert!(
            !shown.contains("streaming") && !shown.contains("Turn on"),
            "{shown}"
        );

        let none = host(&[Provider::Claude], &[]);
        let run = run_guided(
            "",
            &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &none,
        );
        let shown = run.shown;
        assert!(run.plan.expect("answered").changes.is_empty());
        assert!(
            shown.contains("codex: not found on your PATH, left off"),
            "{shown}"
        );

        let original = "[providers.codex]\nstreaming = true\n";
        let run = run_guided(
            original,
            &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &found,
        );
        run.plan.expect("answered");
        assert!(run.draft.settings().satisfies(Key::Codex));
        assert!(
            run.shown.contains("codex: found, already on"),
            "{}",
            run.shown
        );
    }

    /// Yes turns on the six actions and leaves Renew; No turns them and Renew off.
    #[test]
    fn one_floor_question_turns_the_actions_on_or_off_together() {
        let quiet = host(&[Provider::Claude], &[]);
        let run = run_guided(
            "",
            &[(FLOOR, "y"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &quiet,
        );
        assert_eq!(
            run.plan.expect("answered").changes,
            [
                change(Key::Answer, "on"),
                change(Key::Hire, "on"),
                change(Key::Control, "on"),
                change(Key::Goal, "on"),
                change(Key::Retire, "on"),
                change(Key::Terminal, "on"),
            ]
        );

        let all_on = "[floor.answer]\nenabled = true\n[floor.hire]\nenabled = true\n[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.renew]\nenabled = true\n[floor.retire]\nenabled = true\n[floor.terminal]\nenabled = true\n";
        let run = run_guided(
            all_on,
            &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &quiet,
        );
        let shown = run.shown;
        run.plan.expect("answered");
        for key in ACTIONS.into_iter().chain([Key::Renew]) {
            assert!(!run.draft.settings().satisfies(key), "{key}:\n{shown}");
        }
        assert!(run.draft.settings().problems().is_empty(), "{shown}");
    }

    /// A file that already mixes the switches keeps its mix and is told so, with no question.
    #[test]
    fn a_mixed_floor_is_kept_and_not_asked_about() {
        let mixed = "[floor.answer]\nenabled = true\n";
        let run = run_guided(
            mixed,
            &[("Hire a first agent? [Y/n]", "n")],
            None,
            &host(&[Provider::Claude], &[]),
        );
        let shown = run.shown;
        assert!(run.plan.expect("answered").changes.is_empty(), "{shown}");
        assert!(shown.contains("keeps that choice."), "{shown}");
        assert!(!shown.contains("Let the Floor act"), "{shown}");
        assert!(run.draft.settings().satisfies(Key::Answer));
        assert!(!run.draft.settings().satisfies(Key::Hire));
    }

    /// The floor cap is asked only for an engine that is on and has an API key in this process.
    #[test]
    fn the_floor_cap_is_asked_only_beside_an_api_key() {
        let cap = "Daily dollar cap for the whole floor (Enter for none):";
        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                (cap, "25"),
                ("Hire a first agent? [Y/n]", "n"),
            ],
            None,
            &host(&[Provider::Claude], &[Provider::Claude]),
        );
        assert_eq!(
            run.plan.expect("answered").changes,
            [change(Key::FloorUsdPerDay, "25")]
        );

        for billed in [&[][..], &[Provider::Codex][..]] {
            let run = run_guided(
                "",
                &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
                None,
                &host(&[Provider::Claude], billed),
            );
            assert!(!run.shown.contains("Daily dollar cap"), "{}", run.shown);
            assert!(run.plan.expect("answered").changes.is_empty());
        }

        let run = run_guided(
            "[budget]\nfloor_usd_per_day = 9\n",
            &[(FLOOR, "n"), ("Hire a first agent? [Y/n]", "n")],
            None,
            &host(&[Provider::Claude], &[Provider::Claude]),
        );
        assert!(!run.shown.contains("Daily dollar cap"), "{}", run.shown);
        assert!(run.shown.contains("The floor's daily cap stays at $9."));
        run.plan.expect("answered");
    }

    /// Each kind of repository suggests its own check, and a repository that names none suggests
    /// nothing.
    #[test]
    fn a_repository_suggests_its_check() {
        let suggest =
            |files: &[(&str, &str)]| suggest_check(repo(files).path()).map(|argv| argv.join(" "));
        assert_eq!(
            suggest(&[("Cargo.toml", "")]).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            suggest(&[("package.json", r#"{"scripts":{"test":"jest"}}"#)]).as_deref(),
            Some("npm test")
        );
        assert_eq!(
            suggest(&[(
                "package.json",
                r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#
            )]),
            None
        );
        assert_eq!(suggest(&[("package.json", r#"{"name":"x"}"#)]), None);
        assert_eq!(suggest(&[("go.mod", "")]).as_deref(), Some("go test ./..."));
        assert_eq!(
            suggest(&[("pyproject.toml", "")]).as_deref(),
            Some("pytest")
        );
        assert_eq!(suggest(&[("pytest.ini", "")]).as_deref(), Some("pytest"));
        assert_eq!(
            suggest(&[("Makefile", "build:\n\ttrue\ntest:\n\ttrue\n")]).as_deref(),
            Some("make test")
        );
        assert_eq!(suggest(&[("Makefile", "build:\n\ttrue\n")]), None);
        assert_eq!(suggest(&[]), None);
    }

    /// The check question offers the suggestion, takes another command, and skips on `-`; with
    /// no suggestion Enter skips enrolling.
    #[test]
    fn the_check_question_offers_skips_and_takes_another_command() {
        let quiet = host(&[Provider::Claude], &[]);
        let go = repo(&[("go.mod", "")]);
        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                (
                    "Completion check for this repository [go test ./...] (- to skip):",
                    "make check",
                ),
                ("Hire a first agent? [Y/n]", "n"),
            ],
            Some(go.path()),
            &quiet,
        );
        let plan = run.plan.expect("answered");
        assert_eq!(plan.check.expect("a check").argv, ["make", "check"]);
        assert_eq!(plan.enroll.expect("enrolled").check_label, "make check");

        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                ("(- to skip):", "-"),
                ("Hire a first agent? [Y/n]", "n"),
            ],
            Some(go.path()),
            &quiet,
        );
        let plan = run.plan.expect("answered");
        assert!(plan.check.is_none() && plan.enroll.is_none());

        let bare = repo(&[]);
        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                ("Completion check for this repository (- to skip):", ""),
                ("Hire a first agent? [Y/n]", "n"),
            ],
            Some(bare.path()),
            &quiet,
        );
        assert!(run.plan.expect("answered").enroll.is_none());
        assert!(!run.shown.contains("Name the check"), "{}", run.shown);
    }

    /// The engine is asked only when more than one is on; a goal asks for a check with the
    /// suggestion when none exists yet; caps are asked only beside an API key.
    #[test]
    fn the_first_hire_asks_only_what_it_needs() {
        let go = repo(&[("go.mod", "")]);
        let all = host(&[Provider::Claude, Provider::Codex], &[Provider::Codex]);
        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                ("Enter for none):", ""),
                ("(- to skip):", "-"),
                ("Hire a first agent? [Y/n]", ""),
                ("Name [Kim]:", "Ada"),
                ("Engine (claude, codex) [claude]:", "codex"),
                ("Goal (Enter for none):", "fix the build"),
                ("Completion check [go test ./...]:", ""),
                ("Whole dollars per day (Enter for none):", "3"),
                ("Tokens per day (Enter for none):", ""),
            ],
            Some(go.path()),
            &all,
        );
        let plan = run.plan.expect("answered");
        assert_eq!(plan.check.expect("a check").argv, ["go", "test", "./..."]);
        let hire = plan.hire.expect("hired");
        assert_eq!(
            (
                hire.name.as_str(),
                hire.provider,
                hire.usd_per_day,
                hire.tokens_per_day
            ),
            ("Ada", Provider::Codex, Some(3), None)
        );
        assert!(plan.enroll.is_none());

        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                ("Hire a first agent? [Y/n]", ""),
                ("Name [Kim]:", ""),
                ("Goal (Enter for none):", ""),
            ],
            None,
            &host(&[Provider::Claude], &[]),
        );
        assert!(!run.shown.contains("Engine ("), "{}", run.shown);
        assert!(!run.shown.contains("Whole dollars"), "{}", run.shown);
        run.plan.expect("answered");
    }

    /// With no engine program on the PATH, hiring defaults to No.
    #[test]
    fn hiring_defaults_to_no_without_an_engine_program() {
        let run = run_guided(
            "",
            &[(FLOOR, "n"), ("Hire a first agent? [y/N]", "")],
            None,
            &host(&[], &[]),
        );
        assert!(run.plan.expect("answered").hire.is_none());
    }

    /// The workspace sentence says the Floor hires into it only while Hire is on.
    #[test]
    fn enrolling_says_hire_is_off_when_the_floor_is_declined() {
        let bare = repo(&[]);
        let quiet = host(&[Provider::Claude], &[]);
        let questions = [
            (FLOOR, "n"),
            ("(- to skip):", "-"),
            ("Hire a first agent? [Y/n]", "n"),
        ];
        let run = run_guided("", &questions, Some(bare.path()), &quiet);
        assert!(
            run.shown
                .contains("Enrolled, the Floor can hire agents into it once floor.hire is on."),
            "{}",
            run.shown
        );
        let run = run_guided(
            "",
            &[(FLOOR, "y"), questions[1], questions[2]],
            Some(bare.path()),
            &quiet,
        );
        assert!(
            run.shown
                .contains("Enrolled, the Floor can hire agents into it. "),
            "{}",
            run.shown
        );
    }

    /// A lone `-` is not a program when the check cannot be skipped.
    #[test]
    fn a_required_check_rejects_a_lone_dash() {
        let go = repo(&[("go.mod", "")]);
        let run = run_guided(
            "",
            &[
                (FLOOR, "n"),
                ("(- to skip):", "-"),
                ("Hire a first agent? [Y/n]", ""),
                ("Name [Kim]:", ""),
                ("Goal (Enter for none):", "ship it"),
                ("Completion check [go test ./...]:", "-"),
                ("Type the program and its arguments.", "make check"),
            ],
            Some(go.path()),
            &host(&[Provider::Claude], &[]),
        );
        let plan = run.plan.expect("answered");
        assert_eq!(plan.check.expect("a check").argv, ["make", "check"]);
    }

    /// The input ending is a stop, never a default: nothing is planned.
    #[test]
    fn an_ended_input_stops_without_a_plan() {
        let run = run_guided("", &[(FLOOR, "n")], None, &host(&[Provider::Claude], &[]));
        assert!(matches!(run.plan, Err(Stopped::Ended)), "{:?}", run.plan);
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
