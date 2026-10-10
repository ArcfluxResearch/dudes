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
) -> Result<Plan, Stopped> {
    let mut plan = Plan::default();
    term.say(&format!(
        "Dudes quickstart sets up {}, starts the daemon and prints a link to the Floor. Enter \
         takes the answer in capitals or in brackets.",
        config.display()
    ))?;
    engines_step(term, draft, &mut plan)?;
    floor_step(term, draft, &mut plan)?;
    cap_step(term, draft, &mut plan)?;
    if let Some(cwd) = cwd.filter(|cwd| cwd.join(".git").exists()) {
        workspace_step(term, draft.settings(), cwd, &mut plan)?;
    }
    hire_step(term, draft.settings(), &mut plan)?;
    Ok(plan)
}

fn engines_step(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    plan: &mut Plan,
) -> Result<(), Stopped> {
    term.say("\nEngines. Claude Code is always on.")?;
    if switch(term, draft, plan, Key::Codex, "Turn on Codex?")? {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            term.say("Codex streaming uses a separate sign-in managed by the daemon.")?;
            switch(
                term,
                draft,
                plan,
                Key::CodexStreaming,
                "Turn on Codex streaming?",
            )?;
        } else {
            term.say("Codex streaming requires macOS or Linux; its current setting is unchanged.")?;
        }
    }
    switch(term, draft, plan, Key::Pi, "Turn on Pi?")?;
    Ok(())
}

fn floor_step(term: &mut Terminal<'_>, draft: &mut Draft, plan: &mut Plan) -> Result<(), Stopped> {
    term.say(
        "\nFloor actions. Each lets the Floor, in your browser, act on an agent, and each is off \
         unless you turn it on.",
    )?;
    switch(
        term,
        draft,
        plan,
        Key::Answer,
        "Answer an agent's question from the Floor?",
    )?;
    term.say(
        "With Hire on, anyone holding a Floor session can start agents that act with your \
         account's full reach.",
    )?;
    switch(
        term,
        draft,
        plan,
        Key::Hire,
        "Hire an agent from the Floor?",
    )?;
    let control = switch(
        term,
        draft,
        plan,
        Key::Control,
        "Pause, resume and rename an agent from the Floor?",
    )?;
    for (key, question) in [
        (Key::Goal, "Change an agent's goal from the Floor?"),
        (
            Key::Renew,
            "Start a new goal from the Floor when an agent's goal has nothing left?",
        ),
        (Key::Retire, "Retire an agent from the Floor?"),
    ] {
        let settings = draft.settings();
        let asked = control
            && (key != Key::Renew
                || (settings.satisfies(Key::Goal) && settings.satisfies(Key::Hire)));
        if asked && key == Key::Renew {
            term.say(
                "With Renew on, anyone holding a Floor session can give an agent whose goal has \
                 nothing left a new goal with its workspace's turns and ceilings, each time it runs \
                 out.",
            )?;
        }
        if asked {
            switch(term, draft, plan, key, question)?;
        } else if settings.satisfies(key) {
            set(draft, plan, key, false)?;
        }
    }
    switch(
        term,
        draft,
        plan,
        Key::Terminal,
        "Attach a terminal to an agent from the Floor?",
    )?;
    Ok(())
}

fn cap_step(term: &mut Terminal<'_>, draft: &mut Draft, plan: &mut Plan) -> Result<(), Stopped> {
    term.say(
        "\nSpending cap. Subscription plans have their own limits, so most people skip this. \
         Set one when an engine bills an API key for each token.",
    )?;
    if draft.settings().satisfies(Key::FloorUsdPerDay) {
        return Ok(term.say(&format!(
            "The floor's daily cap stays at ${}.",
            draft.settings().shown(Key::FloorUsdPerDay)
        ))?);
    }
    if term.yes("Set a spending cap (for API-key billing)?", false)? {
        let change = term.value("Daily dollar cap for the whole floor", None, floor_cap)?;
        draft.apply(&change)?;
        plan.changes.push(change);
    }
    Ok(())
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
        "\nWorkspace. {} is a git repository. Enrolled, {hires}. A workspace needs a completion \
         check: a command the daemon runs itself to decide that a goal is done, typed as a \
         program and its arguments separated by spaces.",
        cwd.display()
    ))?;
    let Some(argv) = term.value("Completion check (Enter to skip enrolling)", None, words)? else {
        return Ok(());
    };
    let check_label = term.value(
        "Name the check for the Floor",
        Some(&argv.join(" ")),
        display_text,
    )?;
    term.say(
        "A goal normally needs a check that fails until that goal is met. Add a check for each \
         kind of goal with dudes workspace check add.",
    )?;
    plan.check = Some(check(argv));
    plan.enroll = Some(Enroll {
        check_label,
        department: DEFAULT_DEPARTMENT.to_owned(),
        agent_usd_per_day: None,
        goal_usd: None,
    });
    Ok(())
}

fn hire_step(term: &mut Terminal<'_>, file: &Settings, plan: &mut Plan) -> Result<(), Stopped> {
    term.say(
        "\nFirst hire. An agent with a goal starts working at once, and its engine charges for \
         every turn.",
    )?;
    if !term.yes("Hire a first agent now?", false)? {
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
    let goal = term.value(
        "Goal (Enter to hire without one; an agent without a goal takes no turn)",
        None,
        |text| Ok((!text.is_empty()).then(|| text.to_owned())),
    )?;
    if goal.is_some() && plan.check.is_none() {
        term.say(
            "The goal needs a completion check: a command the daemon runs itself to decide that \
             it is done, typed as a program and its arguments separated by spaces.",
        )?;
        let argv = term.value("Completion check", None, |text| {
            words(text)?.ok_or_else(|| "Type the program and its arguments.".to_owned())
        })?;
        plan.check = Some(check(argv));
    }
    let (mut usd_per_day, mut tokens_per_day) = (None, None);
    if term.yes(
        "Give this agent a spending cap (for API-key billing)?",
        false,
    )? {
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

/// Ask whether `key` is on, starting from what the file holds, and apply a change.
fn switch(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    plan: &mut Plan,
    key: Key,
    question: &str,
) -> Result<bool, Stopped> {
    let now = draft.settings().satisfies(key);
    let on = term.yes(&format!("{question} ({key})"), now)?;
    if on != now {
        set(draft, plan, key, on)?;
    }
    Ok(on)
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

    /// Run [`guided`] over `answers`, one line each, against an empty operator file.
    /// Run [`guided`] against an empty operator file, answering each question in turn, and check
    /// that the questions were asked in that order.
    fn run_guided(
        script: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> (Result<Plan, Stopped>, Draft, String) {
        let home = tempfile::tempdir().expect("a home");
        let config = home.path().join("config.toml");
        let mut draft = Draft::open(&config).expect("an absent file opens empty");
        let answers: Vec<&str> = script.iter().map(|(_, answer)| *answer).collect();
        let mut reader = io::Cursor::new(answers.join("\n") + "\n");
        let mut out = Vec::new();
        let plan = guided(
            &mut Terminal::new(&mut reader, &mut out),
            &mut draft,
            &config,
            cwd,
        );
        let shown = String::from_utf8(out).expect("the questions are text");
        let mut rest = shown.as_str();
        for (question, _) in script {
            let at = rest
                .find(question)
                .unwrap_or_else(|| panic!("`{question}` was not asked next in:\n{shown}"));
            rest = &rest[at + question.len()..];
        }
        (plan, draft, shown)
    }

    fn change(key: Key, text: &str) -> Change {
        Change::set(key, text).expect("a value the key takes")
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn codex_streaming_is_opt_in_and_keeps_the_current_choice() {
        for (original, answers, expected, prompted) in [
            ("", "\n\n", false, false),
            ("", "y\n\n\n", false, true),
            ("", "y\ny\n\n", true, true),
            (
                "[providers.codex]\nstreaming = false # keep\n",
                "\n\n\n",
                false,
                true,
            ),
            (
                "[providers.codex]\nstreaming = true # keep\n",
                "\n\n\n",
                true,
                true,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let config = directory.path().join("config.toml");
            std::fs::write(&config, original).unwrap();
            let mut draft = Draft::open(&config).unwrap();
            let mut plan = Plan::default();
            let mut input = io::Cursor::new(answers);
            let mut output = Vec::new();
            engines_step(
                &mut Terminal::new(&mut input, &mut output),
                &mut draft,
                &mut plan,
            )
            .unwrap();
            let shown = String::from_utf8(output).unwrap();
            assert_eq!(
                shown.contains("(providers.codex.streaming)"),
                prompted,
                "{shown}"
            );
            if prompted {
                assert!(shown.contains("separate sign-in"), "{shown}");
                let default = if original.contains("streaming = true") {
                    "[Y/n]"
                } else {
                    "[y/N]"
                };
                assert!(
                    shown.contains(&format!("(providers.codex.streaming) {default}")),
                    "{shown}"
                );
            }
            draft.write().unwrap();
            let written = std::fs::read_to_string(config).unwrap();
            assert_eq!(written.contains("streaming = true"), expected);
            if !original.is_empty() {
                assert_eq!(written, original);
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn unsupported_streaming_is_not_offered_or_reconfigured() {
        for value in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let config = directory.path().join("config.toml");
            let original = format!("[providers.codex]\nstreaming = {value}\n");
            std::fs::write(&config, &original).unwrap();
            let mut draft = Draft::open(&config).unwrap();
            let mut input = io::Cursor::new("\n\n");
            let mut output = Vec::new();
            engines_step(
                &mut Terminal::new(&mut input, &mut output),
                &mut draft,
                &mut Plan::default(),
            )
            .unwrap();
            let shown = String::from_utf8(output).unwrap();
            assert!(!shown.contains("Turn on Codex streaming?"));
            assert!(shown.contains("requires macOS or Linux"));
            draft.write().unwrap();
            assert_eq!(std::fs::read_to_string(config).unwrap(), original);
        }
    }

    /// Each answer lands in the plan and in the draft, a default is taken on Enter, and a Floor
    /// action that needs `floor.control` is not asked while control is off.
    #[test]
    fn the_guided_questions_build_the_plan() {
        let repo = tempfile::tempdir().expect("a repository");
        std::fs::create_dir(repo.path().join(".git")).expect("a .git directory");
        let mut script = vec![
            ("(providers.codex) [y/N]", "y"),
            ("(providers.codex.streaming) [y/N]", ""),
            ("(providers.pi) [y/N]", ""),
            ("(floor.answer) [y/N]", "y"),
            ("(floor.hire) [y/N]", "y"),
            ("(floor.control) [y/N]", "n"),
            ("(floor.terminal) [y/N]", ""),
            ("Set a spending cap (for API-key billing)? [y/N]", ""),
            (
                "Completion check (Enter to skip enrolling):",
                "cargo test --quiet",
            ),
            ("Name the check for the Floor [cargo test --quiet]:", ""),
            ("Hire a first agent now? [y/N]", "y"),
            ("Name [Kim]:", ""),
            ("Engine (claude, codex) [claude]:", "codex"),
            ("Goal (Enter to hire without one", ""),
            (
                "Give this agent a spending cap (for API-key billing)? [y/N]",
                "",
            ),
        ];
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            script.remove(1);
        }
        let (plan, draft, shown) = run_guided(&script, Some(repo.path()));

        assert_eq!(
            plan.expect("every question was answered"),
            Plan {
                changes: vec![
                    change(Key::Codex, "on"),
                    change(Key::Answer, "on"),
                    change(Key::Hire, "on"),
                ],
                check: Some(Check {
                    argv: vec!["cargo".to_owned(), "test".to_owned(), "--quiet".to_owned()],
                    timeout_s: 600,
                    hop_cap: 5,
                }),
                enroll: Some(Enroll {
                    check_label: "cargo test --quiet".to_owned(),
                    department: "engineering".to_owned(),
                    agent_usd_per_day: None,
                    goal_usd: None,
                }),
                hire: Some(Hire {
                    name: "Kim".to_owned(),
                    role: "engineer".to_owned(),
                    provider: Provider::Codex,
                    goal: None,
                    usd_per_day: None,
                    tokens_per_day: None,
                }),
                restart: false,
            }
        );
        assert!(draft.settings().satisfies(Key::Codex));
        assert!(!draft.settings().satisfies(Key::Pi));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(shown.contains("Codex's command sandbox permits writes in this workspace, its permitted temporary directories and its own outgoing mailbox. The completion check runs this workspace's code with no sandbox."));
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(shown.contains("Codex's command sandbox permits writes in this workspace and its permitted temporary directories. Mail is unavailable on this platform. The completion check runs this workspace's code with no sandbox."));
        assert!(
            !shown.contains("(floor.goal)")
                && !shown.contains("(floor.renew)")
                && !shown.contains("(floor.retire)"),
            "goal, renew and retire need floor.control, which is off: {shown}"
        );
        let grant = shown
            .find("With Hire on, anyone holding a Floor session can start agents that act with your account's full reach.")
            .expect("the Hire question says what it grants");
        assert!(grant < shown.find("(floor.hire)").expect("the Hire question"));
        assert!(
            shown.contains("Enrolled, the Floor can hire agents into it. "),
            "{shown}"
        );
    }

    /// Renewal is asked right after goal change, and only while goal change and Hire are both on;
    /// a renewal left on beside a goal change turned off is turned off with it.
    #[test]
    fn renewal_is_asked_only_beside_goal_change_and_hire() {
        let script = [
            ("(providers.codex) [y/N]", ""),
            ("(providers.pi) [y/N]", ""),
            ("(floor.answer) [y/N]", ""),
            ("(floor.hire) [y/N]", "y"),
            ("(floor.control) [y/N]", "y"),
            ("(floor.goal) [y/N]", "y"),
            ("(floor.renew) [y/N]", "y"),
            ("(floor.retire) [y/N]", ""),
            ("(floor.terminal) [y/N]", ""),
            ("Set a spending cap (for API-key billing)? [y/N]", ""),
            ("Hire a first agent now? [y/N]", ""),
        ];
        let (plan, draft, shown) = run_guided(&script, None);
        assert_eq!(
            plan.expect("every question was answered").changes,
            [
                change(Key::Hire, "on"),
                change(Key::Control, "on"),
                change(Key::Goal, "on"),
                change(Key::Renew, "on"),
            ]
        );
        assert!(draft.settings().problems().is_empty(), "{shown}");
        let grant = shown
            .find("With Renew on, anyone holding a Floor session can give an agent whose goal has nothing left a new goal with its workspace's turns and ceilings, each time it runs out.")
            .expect("the Renew question says what it grants");
        assert!(grant < shown.find("(floor.renew)").expect("the Renew question"));

        let without_goal = [
            ("(providers.codex) [y/N]", ""),
            ("(providers.pi) [y/N]", ""),
            ("(floor.answer) [y/N]", ""),
            ("(floor.hire) [y/N]", "y"),
            ("(floor.control) [y/N]", "y"),
            ("(floor.goal) [y/N]", ""),
            ("(floor.retire) [y/N]", ""),
            ("(floor.terminal) [y/N]", ""),
            ("Set a spending cap (for API-key billing)? [y/N]", ""),
            ("Hire a first agent now? [y/N]", ""),
        ];
        let (plan, _, shown) = run_guided(&without_goal, None);
        plan.expect("every question was answered");
        assert!(!shown.contains("(floor.renew)"), "{shown}");
    }

    /// With Hire off, enrolling says the Floor hires into the workspace only once Hire is on.
    #[test]
    fn enrolling_without_hire_says_hire_is_off() {
        let repo = tempfile::tempdir().expect("a repository");
        std::fs::create_dir(repo.path().join(".git")).expect("a .git directory");
        let script = [
            ("(providers.codex) [y/N]", ""),
            ("(providers.pi) [y/N]", ""),
            ("(floor.answer) [y/N]", ""),
            ("(floor.hire) [y/N]", ""),
            ("(floor.control) [y/N]", ""),
            ("(floor.terminal) [y/N]", ""),
            ("Set a spending cap (for API-key billing)? [y/N]", ""),
            ("Completion check (Enter to skip enrolling):", ""),
            ("Hire a first agent now? [y/N]", ""),
        ];
        let (plan, _, shown) = run_guided(&script, Some(repo.path()));
        plan.expect("every question was answered");
        assert!(
            shown.contains("Enrolled, the Floor can hire agents into it once floor.hire is on. "),
            "{shown}"
        );
    }

    /// The input ending is a stop, never a default: nothing is planned and nothing is applied.
    #[test]
    fn an_ended_input_stops_without_a_plan() {
        let (plan, draft, _) = run_guided(&[("(providers.codex) [y/N]", "y")], None);
        assert!(matches!(plan, Err(Stopped::Ended)), "{plan:?}");
        assert!(
            draft.settings().satisfies(Key::Codex),
            "the one answer given was applied to the draft, which is never written"
        );
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
