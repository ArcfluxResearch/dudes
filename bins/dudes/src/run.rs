//! Connect, present the command's identity material, render the answer, exit.
//!
//! # The two doors are visible in one function
//!
//! Operator commands read the credential through [`crate::credential::read_operator_credential`]
//! and hand the value to [`crate::call::call_presenting`]. Two named functions, one value between
//! them, checked by `tests/door_shape.rs`. `agent.ask` is the disjoint
//! agent door: it reads the environment's token by reference, sends it as the existing required
//! param, and never opens the operator credential path. The own closing acknowledgement uses the same
//! token-only presentation.
//!
//! # What is refused here, and what is not
//!
//! **Nothing about a role is decided in this process.** A CLI holding no credential still makes the
//! call, and the daemon answers it: presenting nothing resolves to `Unproven`, and what that
//! means and what each method admits are the daemon's to decide, not restated here. A binary that refused locally on the grounds that it holds no credential would
//! have written a third copy of each method's `Requirement`, and would answer differently from the daemon the day the
//! daemon changed.
//!
//! So the only local refusals are the ones that are genuinely local: arguments this parser cannot
//! read, and a socket nothing is listening on.

use std::ffi::OsString;
use std::io::{BufRead, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dudes_proto::budget::{AgentBudget, GoalBudget};
use dudes_proto::goal::Predicate;
use dudes_proto::rpc::agent::{
    HireParams, HireResult, McpResult, RetireParams, WhyResult, ask_params,
};
use dudes_proto::rpc::board::{CardClaimParams, CardCreateResult, CardTransitionParams};
use dudes_proto::rpc::closing::{
    CLOSING_GRACE_SECONDS, CLOSING_PARTICIPANT_LIMIT, CLOSING_REPORT_MAX_BYTES, ClosingParams,
    ClosingReport, ClosingStarted, closing_ack_params,
};
use dudes_proto::rpc::floor::{
    EnrollmentApply, EnrollmentReloadParams, EnrollmentReloadResult, SessionIssueResult,
};
use dudes_proto::rpc::goal::{
    CompletionCheckDeclaration, ENROLLED_CHECK_ID, GoalSetParams, GoalSetResult,
};
use dudes_proto::rpc::journal::JournalAppendedParams;
use dudes_proto::rpc::system::{
    DoctorResult, PingResult, ProviderProbe, SHUTDOWN_CONTAINMENT_UNPROVEN, ShutdownResult, SignIn,
};
use dudes_proto::rpc::{AgentRef, Empty, Method, NotificationKind, Response, RpcError};
use dudes_rpc::{ClientError, Inbound, LocalClient};
use dudes_types::{AgentId, Provider, Tokens, Usd};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::args::{self, BoardCommand, Command, ConfigCommand, Invocation, WorkspaceCommand};
use crate::call::{Answered, answered, call_presenting};
use crate::config::{self, Draft};
use crate::credential::read_operator_credential;
use crate::env::Environment;
use crate::login::{Engine, Login};
use crate::setup::{self, Plan, Stopped, Terminal};
use crate::workspace::{self, Edit, NewWorkspace};
use crate::{exit, render};

mod ticket;

/// The client this binary drives: one local connection, buffered on the read side.
type Socket = LocalClient;

/// Parse, start the daemon for quickstart when needed, call, render — and return the process's exit
/// code.
///
/// Takes its streams as arguments so the whole thing is drivable in-process. The binary is the only
/// place that names `stdout` and `stderr`, which is also what keeps `clippy::print_stdout` answered
/// by construction rather than by an `#[allow]`. `terminal` is the operator's input when both
/// standard input and standard output are a terminal, and `None` otherwise. Quickstart asks its
/// questions there, and retirement without explicit consent confirms there before dispatch.
///
/// Never panics on a daemon's answer: every shape the transport can produce maps to a code in
/// [`crate::exit`].
pub fn run(
    argv: &[OsString],
    env: &Environment,
    terminal: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    // The one help or version request that succeeds is the whole argv: a lone flag. A help token
    // inside a longer argv still falls to the parser below and exits `USAGE`, so a predicate that
    // carries one by accident never passes having sent nothing.
    if let [only] = argv {
        if only == "--help" || only == "-h" {
            let _ = write!(out, "{}", args::USAGE);
            return exit::SUCCESS;
        }
        if only == "--version" || only == "-V" {
            let _ = writeln!(out, "dudes {}", env!("CARGO_PKG_VERSION"));
            return exit::SUCCESS;
        }
    }
    let invocation = match args::parse(argv) {
        Ok(invocation) => invocation,
        // **Help is a parse outcome, never a pre-scan.** It reaches here only from a flag position
        // the parser was standing in — after the `--` split and after every value-taking flag has
        // taken its value — so a predicate's `--help` and a `--text help` are the operand and the
        // text they were typed as, and their call is sent. A whole-argv scan would answer
        // both with the usage text and exit `0` having sent nothing, which would make
        // [`exit::SUCCESS`] mean two different things.
        //
        // **The text goes to stdout because the operator asked for it; the code is
        // [`exit::USAGE`] because nothing was sent.** `SUCCESS` has one meaning —
        // the daemon ran the method and answered (see [`exit::SUCCESS`]) — and a help request did not.
        //
        // **The cost of `0` here is invariant 3, because `dudes` is a predicate program.** A
        // command predicate passes iff its child exits `0` (`bins/duded/src/predicate.rs`'s
        // `judge`), the supervisor stamps that result `Provenance::Observed` unconditionally
        // (`crates/dudes-supervisor/src/effect.rs`'s `on_predicate_done`), and the loop returns
        // `Done` iff the result is observed and passed (`crates/dudes-policy/src/predicate.rs`'s
        // `judge`). So a predicate whose argv carries a help token in a flag position would hand
        // the loop an **observed `Done` for a process that printed this usage text and did
        // nothing** — a goal completed on a usage message, with no second observation able to
        // catch it, since `Observed` is the strongest provenance there is.
        //
        // The two arms stay separate because what distinguishes them is the stream and the
        // message, not the code. `tests/success_means_sent.rs` holds the property over argv.
        Err(args::ArgError::HelpRequested) => {
            let _ = write!(out, "{}", args::USAGE);
            return exit::USAGE;
        }
        // The mistake and where the usage is, not the whole usage text: that buries the one line
        // that says what was wrong.
        Err(error) => {
            let _ = render::local_error(err, &error.to_string());
            let _ = writeln!(err, "{}", args::HINT);
            return exit::USAGE;
        }
    };
    let dispatched = match &invocation.command {
        Command::Quickstart(plan) => {
            quickstart(invocation.json, plan.as_deref(), terminal, env, out, err)
        }
        Command::Start => start_command(invocation.json, env, out, err),
        Command::SelectHire(params) => {
            select_hire(params, invocation.json, terminal, env, out, err)
        }
        Command::Login(provider) => login(*provider, terminal.is_some(), env, out),
        Command::Mail(command) => crate::mail::run(command, env, out)
            .map(|()| exit::SUCCESS)
            .map_err(local),
        Command::Retire { agent, yes } => {
            confirm_retirement(*agent, invocation.json, *yes, terminal, out)
                .and_then(|()| dispatch(&invocation, env, out, err))
        }
        _ => dispatch(&invocation, env, out, err),
    };
    match dispatched {
        Ok(code) => code,
        Err(failure) => {
            let _ = failure.render(invocation.json, err);
            failure.code()
        }
    }
}

fn confirm_retirement(
    agent: AgentId,
    machine: bool,
    yes: bool,
    terminal: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    if yes {
        return Ok(());
    }
    let Some(input) = terminal.filter(|_| !machine) else {
        return Err(local(
            "retire requires --yes with --json or without an interactive terminal",
        ));
    };
    let mut terminal = Terminal::new(input, out);
    terminal
        .say(&format!(
            "Retirement of agent {agent} cannot be undone by resume."
        ))
        .map_err(local)?;
    match terminal.yes(&format!("Retire agent {agent}?"), false) {
        Ok(true) => Ok(()),
        Ok(false) | Err(Stopped::Ended) => Err(local("retirement canceled; no request was sent")),
        Err(error) => Err(local(error)),
    }
}

/// Everything that ends a run without a rendered result.
///
/// A `Response` carrying an `error` is **not** in here: that is an answer, it is rendered as one,
/// and its code is decided in [`crate::call::answered`]. These are the ways there was no answer at
/// all, plus the local read that could have gone wrong before the call.
#[derive(Debug)]
enum Failure {
    /// The socket could not be reached, or the conversation ended without an answer.
    Unreachable(String),
    /// The daemon declined the call.
    Refused(RpcError),
    /// The daemon answered with some other error, or a local read failed.
    Other(Message),
}

/// Either a daemon's typed error or a local message — the two things [`Failure::Other`] can be.
#[derive(Debug)]
enum Message {
    /// The contract's error, so `--json` can emit it.
    Rpc(Box<RpcError>),
    /// A local failure, which has no contract type.
    Local(String),
}

impl Failure {
    /// The same failure, with `note` after what went wrong.
    /// Whether the daemon refused because the goal's check already passes.
    fn check_already_passes(&self) -> bool {
        let error = match self {
            Self::Refused(error) => error,
            Self::Other(Message::Rpc(error)) => &**error,
            _ => return false,
        };
        error
            .data
            .clone()
            .and_then(|data| serde_json::from_value::<dudes_proto::rpc::RefusedData>(data).ok())
            .is_some_and(|data| data.gate == dudes_proto::rpc::RefusalGate::CheckAlreadyPasses)
    }

    fn noted(self, note: &str) -> Self {
        match self {
            Self::Unreachable(message) => Self::Unreachable(format!("{message}; {note}")),
            Self::Other(Message::Local(message)) => {
                Self::Other(Message::Local(format!("{message}; {note}")))
            }
            Self::Refused(mut error) => {
                error.message = format!("{}; {note}", error.message);
                Self::Refused(error)
            }
            Self::Other(Message::Rpc(mut error)) => {
                error.message = format!("{}; {note}", error.message);
                Self::Other(Message::Rpc(error))
            }
        }
    }

    /// What went wrong, without the code or the program name.
    fn message(&self) -> &str {
        match self {
            Self::Unreachable(message) | Self::Other(Message::Local(message)) => message,
            Self::Refused(error) => &error.message,
            Self::Other(Message::Rpc(error)) => &error.message,
        }
    }

    fn code(&self) -> u8 {
        match self {
            Self::Unreachable(_) => exit::UNREACHABLE,
            Self::Refused(_) => exit::REFUSED,
            Self::Other(_) => exit::OTHER,
        }
    }

    fn render(&self, machine: bool, err: &mut dyn Write) -> std::io::Result<()> {
        match self {
            Self::Unreachable(message) | Self::Other(Message::Local(message)) => {
                render::local_error(err, message)
            }
            Self::Refused(error) => render::rpc_error(err, machine, error),
            Self::Other(Message::Rpc(error)) => render::rpc_error(err, machine, error),
        }
    }
}

/// A conversation that failed is unreachable, not a refusal.
///
/// [`ClientError`] covers exactly the ways the conversation itself broke — the frame, the stream,
/// a close with no answer, an answer that was not a response. None of them is the daemon declining
/// anything, so all of them are [`exit::UNREACHABLE`]: a script that retries after starting a daemon
/// is doing the right thing for every one.
impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        Self::Unreachable(error.to_string())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one function routes every command to its door, and each locally dispatched command adds its arm"
)]
fn dispatch(
    invocation: &Invocation,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let hire = match &invocation.command {
        Command::Hire(params) => Some(Invocation {
            json: invocation.json,
            command: Command::Hire(Box::new(HireParams {
                cwd: canonical_cwd(&params.cwd, env)?,
                ..(**params).clone()
            })),
        }),
        _ => None,
    };
    let invocation = hire.as_ref().unwrap_or(invocation);
    match &invocation.command {
        Command::Workspace(command) => {
            return workspace_command(command, env, invocation.json, out, err);
        }
        Command::Config(command) => return config_command(command, env, out, err),
        Command::Ticket(command) => return ticket::command(command, env, invocation.json, out),
        Command::Worktree(command) => {
            return worktree_command(command, env, invocation.json, out, err);
        }
        Command::GoalReplace(path) => return replace_goal(path, env, invocation.json, out),
        Command::PauseAll => return pause_all(env, out),
        Command::Stop => return stop_closing(invocation.json, env, out, err),
        Command::Goal(params) => return set_goal(params, invocation, env, out),
        _ => {}
    }

    // `floor open` has no version in its answer, so it asks once before it mints the link.
    if invocation.command == Command::FloorOpen
        && !invocation.json
        && let Ok(ping) = ping_once(env)
    {
        write_restart_nudge(err, &ping.version).map_err(local)?;
    }

    let mut client = connect(env)?;

    if invocation.command == Command::Team {
        let response = if let Some(token) = env.agent_token() {
            client.call(
                Method::TeamRoster,
                Some(serde_json::json!({"token": token})),
            )?
        } else {
            call_presenting(
                &mut client,
                Method::TeamRoster,
                Some(encode(&Empty {})?),
                operator_credential(env)?,
            )?
        };
        return render_answer(invocation, Method::TeamRoster, response, out);
    }

    if let Command::ClosingAck { announcement_lsn } = invocation.command {
        let params = encode(&closing_ack_params(env.agent_token(), announcement_lsn))?;
        let response = client.call(Method::SystemClosingTime, Some(params))?;
        let result = typed::<Empty>(answer_result(response)?)?;
        render::closing_acknowledged(out, invocation.json, result).map_err(local)?;
        return Ok(exit::SUCCESS);
    }

    if let Command::Memory(command) = &invocation.command {
        let agent = match command {
            crate::args::MemoryCommand::Append { agent, .. }
            | crate::args::MemoryCommand::Read { agent, .. }
            | crate::args::MemoryCommand::Search { agent, .. } => *agent,
        };
        let (method, mut params) = match command {
            crate::args::MemoryCommand::Append { text, .. } => (
                Method::MemoryAppend,
                encode(&dudes_proto::rpc::memory::MemoryAppendParams {
                    agent,
                    text: text.clone(),
                    token: None,
                })?,
            ),
            crate::args::MemoryCommand::Read { since_lsn, .. } => (
                Method::MemoryRead,
                encode(&dudes_proto::rpc::memory::MemoryReadParams {
                    agent,
                    since_lsn: *since_lsn,
                    token: None,
                })?,
            ),
            crate::args::MemoryCommand::Search { options, .. } => (
                Method::MemorySearch,
                encode(&dudes_proto::rpc::memory::MemorySearchParams {
                    query: options.query().to_owned(),
                    agent,
                    since_lsn: options.since_lsn(),
                    limit: Some(options.limit() as u64),
                    token: None,
                })?,
            ),
        };
        if agent.is_none()
            && let Some(token) = env.agent_token()
        {
            params["token"] = encode(token)?;
        }
        let response = if agent.is_some() {
            call_presenting(&mut client, method, Some(params), operator_credential(env)?)?
        } else {
            client.call(method, Some(params))?
        };
        return render_answer(invocation, method, response, out);
    }

    // The agent door. It reads no operator credential: its sole presentation is the token the
    // supervisor placed in this managed child's environment.
    if let Command::Ask { text } = &invocation.command {
        let params = encode(&ask_params(env.agent_token(), text))?;
        let response = client.call(Method::AgentAsk, Some(params))?;
        return render_answer(invocation, Method::AgentAsk, response, out);
    }

    let cred = operator_credential(env)?;

    // `journal.tail` is a subscription and not a call: its backfill reaches the peer *before* the
    // response to the call that asked for it, so it reads frames rather than one answer.
    if let Command::Tail { params, follow } = &invocation.command {
        return tail(&mut client, invocation, cred, params, *follow, out, err);
    }

    let (method, params) = wire(&invocation.command)?;

    // --- the presentation half. It opens nothing and knows no path. -----------------------------
    let response = call_presenting(&mut client, method, params, cred)?;
    let ping_version = (method == Method::SystemPing && !invocation.json)
        .then(|| response.result.clone())
        .flatten()
        .and_then(|result| serde_json::from_value::<PingResult>(result).ok())
        .map(|ping| ping.version);
    if method == Method::SystemShutdown && response.error.is_none() {
        let shutdown = typed::<ShutdownResult>(response.result.clone().unwrap_or(Value::Null))?;
        drop(client);
        if let Some(warning) = await_stopped(shutdown)? {
            let _ = render::warning(err, warning);
        }
    }

    let hired = (method == Method::AgentHire)
        .then(|| response.result.clone())
        .flatten()
        .and_then(|result| serde_json::from_value::<HireResult>(result).ok());
    let status = render_answer(invocation, method, response, out)?;
    if status == exit::SUCCESS && invocation.command == Command::FloorOpen {
        project_hint(env, invocation.json, out)?;
    }
    if let Some(version) = ping_version {
        write_restart_nudge(err, &version).map_err(local)?;
    }
    if let (Some(hired), Command::Hire(params)) = (hired, &invocation.command) {
        if params.checkout.is_none() {
            warn_if_sharing(env, hired.agent, err);
        }
        if !invocation.json {
            warn_if_no_project(env, hired.agent, err);
        }
    }
    Ok(status)
}

fn select_hire(
    params: &HireParams,
    machine: bool,
    terminal: Option<&mut dyn BufRead>,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let config = operator_config(env)?;
    let check = match checked_reload(env, config)? {
        ReloadCheck::Known(check) => check,
        ReloadCheck::Unknown(mismatch) => return Err(local(mismatch.with_advice())),
    };
    let selected = match check.workspaces.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        ids => {
            let input = terminal.filter(|_| !machine).ok_or_else(|| local(
                "More than one project is enrolled. Run `dudes hire` in a terminal to choose one, or pass --cwd <project folder>.",
            ))?;
            let mut term = Terminal::new(input, out);
            Some(
                term.value(&format!("Project ({})", ids.join(", ")), None, |text| {
                    ids.iter()
                        .find(|id| id.as_str() == text)
                        .cloned()
                        .ok_or_else(|| format!("Choose one of {}.", ids.join(", ")))
                })
                .map_err(local)?,
            )
        }
    };
    let cwd = if let Some(id) = selected {
        if check.removed.contains(&id) || check.changed.iter().any(|changed| changed.id == id) {
            return Err(local(
                "Project settings have changed. Run `dudes workspace reload` before hiring, or pass --cwd <project folder>.",
            ));
        }
        workspace::directory(config, &id).map_err(local)?
    } else {
        PathBuf::from(".")
    };
    dispatch(
        &Invocation {
            json: machine,
            command: Command::Hire(Box::new(HireParams {
                cwd,
                ..params.clone()
            })),
        },
        env,
        out,
        err,
    )
}

fn warn_if_no_project(env: &Environment, agent: AgentId, err: &mut dyn Write) {
    let Ok(agents) =
        operator_call(env, Method::AgentList, None).and_then(typed::<Vec<dudes_proto::AgentView>>)
    else {
        return;
    };
    if let Some(hired) = agents
        .iter()
        .find(|hired| hired.id == agent && hired.workspace.is_none())
    {
        let advice = if no_projects(env) {
            setup::NO_PROJECT_HINT
        } else {
            "Hire a new agent in a project with `dudes hire` to take tickets."
        };
        let _ = render::warning(
            err,
            &format!(
                "{} has no project and can't take tickets. {}",
                render::Escaped(&hired.name),
                advice
            ),
        );
    }
}

/// `dudes goal`: say first that the daemon runs the check once when it will, then make the call,
/// and give a refusal of the check the remedy this CLI has for it.
fn set_goal(
    params: &GoalSetParams,
    invocation: &Invocation,
    env: &Environment,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let mut client = connect(env)?;
    let cred = operator_credential(env)?;
    if let Predicate::Command { timeout_s, .. } = &params.predicate
        && !params.may_already_pass
        && !invocation.json
    {
        writeln!(out, "{}", render::goal_check_runs_first(*timeout_s)).map_err(local)?;
    }
    let response = call_presenting(&mut client, Method::GoalSet, Some(encode(params)?), cred)?;
    // `--json` keeps the daemon's error as the contract states it.
    let explain = |error: &mut RpcError| {
        if let Some(text) = render::goal_refusal(error).filter(|_| !invocation.json) {
            error.message = text;
        }
    };
    render_answer(invocation, Method::GoalSet, response, out).map_err(|failure| match failure {
        Failure::Refused(mut error) => {
            explain(&mut error);
            Failure::Refused(error)
        }
        Failure::Other(Message::Rpc(mut error)) => {
            explain(error.as_mut());
            Failure::Other(Message::Rpc(error))
        }
        other => other,
    })
}

fn canonical_cwd(typed: &Path, env: &Environment) -> Result<PathBuf, Failure> {
    if typed.as_os_str().is_empty() {
        return Err(local("`--cwd` is empty"));
    }
    let joined = if typed.is_absolute() {
        typed.to_path_buf()
    } else {
        let here = env.current_dir().ok_or_else(|| {
            local(format!(
                "{} is not an absolute path, and the current directory cannot be read",
                typed.display()
            ))
        })?;
        here.join(typed)
    };
    let cwd = joined.canonicalize().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => local(format!("{} does not exist", joined.display())),
        _ => local(format!("{}: {error}", joined.display())),
    })?;
    if cwd.is_dir() {
        Ok(cwd)
    } else {
        Err(local(format!("{} is not a directory", cwd.display())))
    }
}

/// A hire into a directory another live agent already records is admitted, and the
/// operator is told. A failed lookup says nothing, because the hire already succeeded.
fn warn_if_sharing(env: &Environment, agent: dudes_types::AgentId, err: &mut dyn Write) {
    let Ok(result) = operator_call(env, Method::AgentList, None) else {
        return;
    };
    let Ok(agents) = typed::<Vec<dudes_proto::AgentView>>(result) else {
        return;
    };
    if agents
        .iter()
        .any(|row| row.id == agent && row.shares_checkout)
    {
        let _ = render::warning(
            err,
            "another live agent works in this directory, so their edits and checkouts will collide \
             with this agent's; hire with --worktree for a tree of its own",
        );
    }
}

/// `dudes worktree list` and `remove`.
///
/// This CLI starts exactly three programs, so it does not run git: while the tree
/// exists, `remove` prints the exact `git worktree remove` command for the operator to run and
/// review, and once the tree is gone it records the removal. The daemon never runs git in a tree an
/// agent wrote, and it refuses the record while the path still exists.
fn worktree_command(
    command: &crate::args::WorktreeCommand,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    use dudes_proto::WorktreeViewState;
    use dudes_proto::rpc::worktree::{RemovedParams, WorktreeEntry};
    let entries = || -> Result<Vec<WorktreeEntry>, Failure> {
        typed(operator_call(env, Method::WorktreeList, None)?)
    };
    match command {
        crate::args::WorktreeCommand::List => {
            render::worktrees(out, machine, &entries()?).map_err(local)?;
            Ok(exit::SUCCESS)
        }
        crate::args::WorktreeCommand::Remove { worktree } => {
            let all = entries()?;
            let Some(entry) = all.iter().find(|entry| entry.worktree == *worktree) else {
                // `worktree.list` omits a removed lease, and the daemon answers the record of a
                // removed lease with success and an unknown one with a refusal, so asking tells
                // "already removed" from "never existed".
                let _: Empty = typed(operator_call(
                    env,
                    Method::WorktreeRemoved,
                    Some(encode(&RemovedParams {
                        worktree: *worktree,
                    })?),
                )?)?;
                render::worktree_already_removed(out, machine, *worktree).map_err(local)?;
                return Ok(exit::SUCCESS);
            };
            if entry.state != WorktreeViewState::Released {
                let _ = render::warning(
                    err,
                    &format!(
                        "worktree {worktree} is {:?}: only a released tree, whose agent has retired \
                         and left no running child, can be removed",
                        entry.state
                    ),
                );
                return Ok(exit::REFUSED);
            }
            if std::fs::symlink_metadata(&entry.path).is_ok() {
                render::worktree_remove_command(err, &entry.origin, &entry.path).map_err(local)?;
                return Ok(exit::REFUSED);
            }
            let _: Empty = typed(operator_call(
                env,
                Method::WorktreeRemoved,
                Some(encode(&RemovedParams {
                    worktree: *worktree,
                })?),
            )?)?;
            render::worktree_removed(out, machine, *worktree, &entry.branch).map_err(local)?;
            Ok(exit::SUCCESS)
        }
    }
}

fn replace_goal(
    path: &Path,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let request = read_goal_replacement(path)?;
    let result = typed::<dudes_proto::rpc::goal::GoalReplaceResult>(operator_call(
        env,
        Method::GoalReplace,
        Some(encode(&request)?),
    )?)?;
    render::goal_replaced(out, machine, &result).map_err(local)?;
    Ok(match result {
        dudes_proto::rpc::goal::GoalReplaceResult::Changed { .. } => exit::SUCCESS,
        dudes_proto::rpc::goal::GoalReplaceResult::Refused { .. } => exit::REFUSED,
        dudes_proto::rpc::goal::GoalReplaceResult::Pending { .. } => exit::OTHER,
    })
}

fn read_goal_replacement(
    path: &Path,
) -> Result<dudes_proto::rpc::goal::GoalReplaceParams, Failure> {
    let file = std::fs::File::open(path).map_err(local)?;
    let mut bytes = Vec::new();
    file.take(dudes_proto::rpc::goal::MAX_GOAL_REPLACE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(local)?;
    dudes_proto::rpc::goal::GoalReplaceParams::decode_strict(&bytes).map_err(|_| {
        local("invalid goal replacement request file; check its strict fields and 128 KiB limit")
    })
}

fn pause_all(env: &Environment, out: &mut dyn Write) -> Result<u8, Failure> {
    let agents =
        typed::<Vec<dudes_proto::AgentView>>(operator_call(env, Method::AgentList, None)?)?;
    let report = pause_each(&agents, |agent| {
        let params = Some(encode(&AgentRef { agent })?);
        let _: Empty = typed(operator_call(env, Method::LoopPause, params)?)?;
        Ok(())
    });
    render::pause_all(out, report.paused, &report.refusals).map_err(local)?;
    if let Some(failure) = report.failure {
        return Err(failure);
    }
    Ok(if report.refusals.is_empty() {
        exit::SUCCESS
    } else {
        exit::REFUSED
    })
}

#[derive(Default)]
struct PauseAllReport {
    paused: usize,
    refusals: Vec<(String, String)>,
    failure: Option<Failure>,
}

/// Keep going after a per-agent refusal or a retirement since the roster read. Other daemon
/// errors stop the batch because their effect may be unknown.
fn pause_each(
    agents: &[dudes_proto::AgentView],
    mut pause: impl FnMut(dudes_types::AgentId) -> Result<(), Failure>,
) -> PauseAllReport {
    let mut report = PauseAllReport::default();
    for agent in agents.iter().filter(|agent| !agent.paused && !agent.halted) {
        match pause(agent.id) {
            Ok(()) => report.paused += 1,
            Err(failure @ Failure::Refused(_)) => {
                report
                    .refusals
                    .push((agent.name.clone(), failure.message().to_owned()));
            }
            Err(Failure::Other(Message::Rpc(error)))
                if error.code == dudes_proto::rpc::ErrorCode::NotFound.code() =>
            {
                report.refusals.push((agent.name.clone(), error.message));
            }
            Err(failure) => {
                report.failure =
                    Some(failure.noted(&format!("after pausing {} agents", report.paused)));
                break;
            }
        }
    }
    report
}

const HARD_STOP_RECEIPT_DEADLINE: Duration = Duration::from_secs(30);
const CLOSING_STOP_DEADLINE: Duration =
    Duration::from_secs(CLOSING_GRACE_SECONDS + HARD_STOP_RECEIPT_DEADLINE.as_secs());

const CONTAINMENT_WARNING: &str = "the daemon stopped and every agent process it started has \
    exited, but it cannot prove that nothing those processes started left their process group or job";

/// Wait for the addressed run's receipt. `Ok(Some(warning))` is a completed stop the operator still
/// needs to hear about.
fn await_stopped(shutdown: ShutdownResult) -> Result<Option<&'static str>, Failure> {
    let local = |message: String| Failure::Other(Message::Local(message));
    let receipt = shutdown.completion.ok_or_else(|| local(
        "the daemon accepted system.shutdown but does not report completion; its exit could not be confirmed".to_owned()
    ))?;
    await_receipt(
        &receipt,
        Instant::now() + HARD_STOP_RECEIPT_DEADLINE,
        "system.shutdown",
        HARD_STOP_RECEIPT_DEADLINE,
    )
}

fn await_receipt(
    receipt: &dudes_proto::rpc::system::ShutdownReceipt,
    deadline: Instant,
    method: &str,
    budget: Duration,
) -> Result<Option<&'static str>, Failure> {
    if !receipt.path.is_absolute() || receipt.run.is_empty() || receipt.run.contains(['\n', '\r']) {
        return Err(local(
            "the daemon returned an invalid shutdown completion receipt".to_owned(),
        ));
    }
    loop {
        match std::fs::read_to_string(&receipt.path) {
            Ok(text) if text == format!("complete {}\n", receipt.run) => return Ok(None),
            Ok(text) => {
                if let Some(reason) = text
                    .strip_prefix(&format!("failed {} ", receipt.run))
                    .and_then(|reason| reason.strip_suffix('\n'))
                {
                    if reason == SHUTDOWN_CONTAINMENT_UNPROVEN {
                        return Ok(Some(CONTAINMENT_WARNING));
                    }
                    return Err(local(format!(
                        "the daemon ended, but its teardown could not be confirmed: {reason}"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(local(format!(
                    "reading {} to see the daemon exit: {error}",
                    receipt.path.display()
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(local(format!(
                "the daemon accepted {method}, but run {} has no completion receipt after {} seconds",
                receipt.run,
                budget.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop_closing(
    machine: bool,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let called_at = Instant::now();
    let socket = env.socket().map_err(local)?.to_path_buf();
    let runtime = env.runtime_dir().map_err(local)?.to_path_buf();
    let (report, warning) = closing_stop(socket, runtime, called_at, CLOSING_STOP_DEADLINE)?;
    if let Some(warning) = warning {
        render::warning(err, warning).map_err(local)?;
    }
    let owed: Vec<_> = report
        .missing
        .iter()
        .filter(|agent| {
            report
                .settled
                .iter()
                .all(|settled| settled.agent != **agent)
        })
        .collect();
    if !owed.is_empty() {
        let ids = owed
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        render::warning(
            err,
            &format!("closing time completed without acknowledgements from: {ids}"),
        )
        .map_err(local)?;
    }
    render::closing_completed(out, machine, &report).map_err(local)?;
    Ok(exit::SUCCESS)
}

fn closing_stop(
    socket: PathBuf,
    runtime: PathBuf,
    called_at: Instant,
    bound: Duration,
) -> Result<(ClosingReport, Option<&'static str>), Failure> {
    let deadline = called_at + bound;
    let response = detached_closing_transport(socket, runtime, called_at, bound)?;
    response
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| local("closing time did not finish within the original call deadline"))?
}

type ClosingStopResult = Result<(ClosingReport, Option<&'static str>), Failure>;

fn detached_closing_transport(
    socket: PathBuf,
    runtime: PathBuf,
    called_at: Instant,
    bound: Duration,
) -> Result<std::sync::mpsc::Receiver<ClosingStopResult>, Failure> {
    let (answer, response) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("closing time".to_owned())
        .spawn(move || {
            let result = (|| {
                let mut client = connect_at(&socket)?;
                let credential = read_operator_credential(&runtime).map_err(local)?;
                let params = Some(encode(&ClosingParams::Announce)?);
                let response =
                    call_presenting(&mut client, Method::SystemClosingTime, params, credential)?;
                drop(client);
                let started = typed::<ClosingStarted>(answer_result(response)?)?;
                await_closing(&started, called_at, bound)
            })();
            let _ = answer.send(result);
        })
        .map_err(local)?;
    drop(worker);
    Ok(response)
}

fn await_closing(
    started: &ClosingStarted,
    called_at: Instant,
    bound: Duration,
) -> Result<(ClosingReport, Option<&'static str>), Failure> {
    if started.announcement.0 == 0
        || started.participants.len() > CLOSING_PARTICIPANT_LIMIT
        || started
            .participants
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != started.participants.len()
        || !started.report.path.is_absolute()
        || started.report.path.parent() != started.completion.path.parent()
        || started.report.run != started.completion.run
    {
        return Err(local("the daemon returned an invalid closing announcement"));
    }
    let wall_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(local)?;
    let now_ms = i64::try_from(wall_now.as_millis()).map_err(local)?;
    let deadline =
        closing_receipt_deadline(called_at, Instant::now(), now_ms, started.deadline, bound);
    let warning = await_receipt(
        &started.completion,
        deadline,
        "system.closingTime",
        CLOSING_STOP_DEADLINE,
    )?;
    let metadata = std::fs::symlink_metadata(&started.report.path)
        .map_err(|error| local(format!("reading the closing report: {error}")))?;
    if !metadata.is_file() {
        return Err(local("the closing report is not a regular file"));
    }
    let file = std::fs::File::open(&started.report.path).map_err(local)?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, CLOSING_REPORT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(local)?;
    if bytes.len() > CLOSING_REPORT_MAX_BYTES {
        return Err(local("the closing report exceeds its byte bound"));
    }
    let report: ClosingReport =
        serde_json::from_slice(&bytes).map_err(|_| local("invalid closing report"))?;
    validate_closing_report(started, &report)?;
    Ok((report, warning))
}

fn closing_receipt_deadline(
    called_at: Instant,
    now: Instant,
    now_ms: i64,
    announced: dudes_types::Timestamp,
    bound: Duration,
) -> Instant {
    let teardown_ms =
        i64::try_from(HARD_STOP_RECEIPT_DEADLINE.as_millis()).expect("teardown bound fits i64");
    let remaining_ms = announced
        .unix_millis()
        .saturating_add(teardown_ms)
        .saturating_sub(now_ms)
        .max(0);
    let remaining =
        Duration::from_millis(u64::try_from(remaining_ms).expect("nonnegative i64 fits u64"));
    let original_limit = called_at + bound;
    now.checked_add(remaining)
        .map_or(original_limit, |wall_limit| wall_limit.min(original_limit))
}

fn validate_closing_report(
    started: &ClosingStarted,
    report: &ClosingReport,
) -> Result<(), Failure> {
    use std::collections::BTreeSet;
    let participants: BTreeSet<_> = started.participants.iter().copied().collect();
    let acknowledged: BTreeSet<_> = report.acknowledged.iter().copied().collect();
    let missing: BTreeSet<_> = report.missing.iter().copied().collect();
    if report.run != started.completion.run
        || report.run != started.report.run
        || report.announcement != started.announcement
        || report.complete <= started.announcement
        || report.participants != started.participants
        || participants.len() != started.participants.len()
        || acknowledged.len() != report.acknowledged.len()
        || missing.len() != report.missing.len()
        || report
            .settled
            .iter()
            .any(|settled| !missing.contains(&settled.agent))
        || report
            .settled
            .iter()
            .map(|settled| settled.agent)
            .collect::<BTreeSet<_>>()
            .len()
            != report.settled.len()
        || !acknowledged.is_disjoint(&missing)
        || acknowledged
            .union(&missing)
            .copied()
            .collect::<BTreeSet<_>>()
            != participants
    {
        return Err(local(
            "the closing report does not match the addressed run, announcement and participant census",
        ));
    }
    Ok(())
}

/// A local failure carrying `error`'s text.
fn local(error: impl std::fmt::Display) -> Failure {
    Failure::Other(Message::Local(error.to_string()))
}

/// `dudes quickstart`. Without a plan and without a terminal to ask one, it starts the daemon and
/// prints the ping, as it always has. Otherwise it settles the plan, writes the operator file once,
/// starts the daemon ([`write_and_start`]), reports the engines, and makes the hire. Only the
/// guided run, with no options, prints the Floor link and asks about a login; a run with options asks
/// nothing and prints the commands instead.
fn quickstart(
    machine: bool,
    plan: Option<&Plan>,
    terminal: Option<&mut dyn BufRead>,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let terminal = terminal.filter(|_| !machine);
    if plan.is_none() && terminal.is_none() {
        let answer = start(env, err)?;
        render::ping(out, machine, &answer).map_err(local)?;
        project_hint(env, machine, out)?;
        return Ok(exit::SUCCESS);
    }
    env.state_dir().and(env.runtime_dir()).map_err(local)?;
    let config = operator_config(env)?;
    let cwd = env.current_dir().and_then(|dir| dir.canonicalize().ok());
    let mut draft = Draft::open(config).map_err(local)?;
    let settled = match (plan, terminal) {
        (Some(plan), _) => scripted(plan.clone(), &mut draft)?,
        (None, Some(input)) => {
            let running = ping_once(env).is_ok();
            let mut term = Terminal::new(input, out);
            match guided(
                &mut term,
                &mut draft,
                config,
                cwd.as_deref(),
                &setup::Host::of(env),
                running,
            ) {
                Ok(plan) => plan,
                Err(stopped @ (Stopped::Ended | Stopped::Declined)) => {
                    render::local_error(err, &stopped.to_string()).map_err(local)?;
                    return Ok(exit::USAGE);
                }
                Err(other) => return Err(local(other)),
            }
        }
        (None, None) => return Err(local("internal: quickstart has no plan to set up")),
    };
    // A check is named on the first goal only when this run enrolls it, so the name is the one
    // the daemon records for the workspace's own check.
    let mut enrolled = None;
    let project = settled.project.as_deref().or(cwd.as_deref());
    if let Some(new) = enrollment(&settled, project)? {
        if let Some(id) = draft.settings().enrolled_at(&new.path) {
            writeln!(
                out,
                "{} is enrolled as {id}; its entry stays as it is.",
                new.path.display()
            )
            .map_err(local)?;
        } else {
            draft.enroll(&new).map_err(local)?;
            let check = enrolled_check(&new.completion_check_label);
            enrolled = Some((new.id, check));
        }
    }
    let answer = write_and_start(env, config, draft, settled.restart, out, err)?;
    render::ping(out, false, &answer).map_err(local)?;
    let extended = typed::<dudes_proto::rpc::system::DoctorMailboxResult>(operator_call(
        env,
        Method::SystemDoctor,
        None,
    )?)?;
    render::mailbox_report(out, &extended.mailbox).map_err(local)?;
    let doctor = extended.doctor;
    setup::report_engines(out, &doctor.providers).map_err(local)?;
    let guided = plan.is_none();
    if guided {
        floor_link(env, out, err)?;
    } else {
        writeln!(out, "Open the Floor with: dudes floor open").map_err(local)?;
    }
    if let Some(hire) = &settled.hire {
        let department = settled
            .enroll
            .as_ref()
            .map_or(setup::DEFAULT_DEPARTMENT, |enroll| {
                enroll.department.as_str()
            });
        hire_first(
            env,
            hire,
            department,
            settled.check.as_ref(),
            project,
            enrolled.as_ref().map(|(id, check)| Enrolled {
                id,
                config,
                check: check.clone(),
            }),
            out,
        )?;
    }
    project_hint(env, machine, out)?;
    login_guidance(&doctor.providers, out)
}

fn login_guidance(providers: &[ProviderProbe], out: &mut dyn Write) -> Result<u8, Failure> {
    for engine in providers
        .iter()
        .filter(|row| row.sign_in == Some(SignIn::SignedOut))
        .filter_map(|row| Engine::of(row.provider))
    {
        writeln!(
            out,
            "{} is signed out. Sign in with: dudes login {}",
            engine.title(),
            engine.provider()
        )
        .map_err(local)?;
    }
    Ok(exit::SUCCESS)
}

/// `dudes login <engine>`: the daemon's answer first, then the engine's own login when it is not
/// signed in.
fn login(
    provider: Provider,
    terminal: bool,
    env: &Environment,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let doctor = operator_call(env, Method::SystemDoctor, None)
        .map_err(|failure| match failure {
            Failure::Unreachable(_) => failure.noted("start it with dudes start"),
            other => other,
        })
        .and_then(typed::<DoctorResult>)?;
    let Some(engine) = Engine::of(provider) else {
        writeln!(out, "{}", crate::login::PI_GUIDANCE).map_err(local)?;
        return Ok(exit::SUCCESS);
    };
    let row = doctor.providers.iter().find(|row| row.provider == provider);
    let managed_login = row
        .filter(|row| row.login_home.is_some())
        .map(|row| Login::from_row(engine, Some(row)))
        .transpose()
        .map_err(local)?;
    if row.and_then(|row| row.sign_in) == Some(SignIn::SignedIn) {
        writeln!(
            out,
            "The daemon reports {} as signed in; nothing was run.",
            engine.title()
        )
        .map_err(local)?;
        return Ok(exit::SUCCESS);
    }
    let login = match managed_login {
        Some(login) => login,
        None => Login::from_row(engine, row).map_err(local)?,
    };
    if !terminal || cfg!(windows) {
        let terminal_name = if cfg!(windows) {
            "PowerShell"
        } else {
            "a terminal"
        };
        writeln!(
            out,
            "Sign in to {} by running this in {terminal_name}: {}",
            engine.title(),
            login.command()
        )
        .map_err(local)?;
        return Ok(exit::SUCCESS);
    }
    run_login(&login, env, out)
}

/// Replace this process with `login`. Returns only when that failed.
fn run_login(login: &Login, env: &Environment, out: &mut dyn Write) -> Result<u8, Failure> {
    if login.working_directory().is_some() {
        writeln!(out, "Running Codex sign-in using Dudes' separate sign-in home.")
    } else {
        writeln!(
        out,
        "Running {}. The daemon starts agents from its own environment, so if it still reports {} \
         signed out afterwards, a setting only this shell has, such as a vendor home directory, \
         does not reach them.",
        login.command(),
        login.engine().title()
        )
    }
    .and_then(|()| out.flush())
    .map_err(local)?;
    #[cfg(unix)]
    {
        let home = login
            .working_directory()
            .or_else(|| env.home())
            .ok_or_else(|| local("HOME is unavailable, so the login has nowhere to run"))?;
        Err(local(format!(
            "starting {}: {}",
            login.command(),
            login.exec(home)
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = env;
        Err(local("a login runs only on Unix"))
    }
}

/// Write `draft`, then start the daemon on it. A running daemon that reads other settings is
/// stopped first only when `restart` is the operator's consent, and only after it has read the
/// new file without refusing it. Until a daemon answers on the new file, a failure puts the old
/// file back.
fn write_and_start(
    env: &Environment,
    config: &Path,
    draft: Draft,
    restart: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<PingResult, Failure> {
    let previous = draft.previous().map(<[u8]>::to_vec);
    let written = draft.write().map_err(local)?;
    for warning in written.warnings() {
        render::warning(err, &warning).map_err(local)?;
    }
    let put_back = |failure: Failure| {
        if !written.wrote {
            return failure;
        }
        match workspace::restore(config, previous.as_deref()) {
            Ok(()) => failure.noted(&format!("{} was put back as it was", config.display())),
            Err(error) => failure.noted(&format!(
                "putting {} back failed: {error}",
                config.display()
            )),
        }
    };
    let mut stopped = false;
    if ping_once(env).is_ok() {
        let differences = match running_differences(env, config).map_err(put_back)? {
            Comparison::Unknown(reason) => vec![reason],
            Comparison::Known {
                settings,
                workspaces,
                ..
            } => workspaces.into_iter().chain(settings).collect(),
        };
        if !differences.is_empty() {
            if !restart {
                let file = if written.wrote { "wrote" } else { "kept" };
                return Err(local(format!(
                    "{file} {}, but the running daemon reads other settings ({}) and reads the \
                     file only when it starts. Nothing was stopped. To apply them, restart it, \
                     which ends any turn it is running: dudes quickstart --restart",
                    config.display(),
                    differences.join(", ")
                )));
            }
            writeln!(
                out,
                "Restarting the daemon so it reads {}.",
                config.display()
            )
            .map_err(local)?;
            hard_stop_daemon(env, err)?;
            stopped = true;
        }
    }
    start(env, err).map_err(|failure| {
        let failure = put_back(failure);
        if stopped && written.wrote && start(env, &mut std::io::sink()).is_ok() {
            return failure.noted("the daemon was started again on it");
        }
        failure
    })
}

/// `dudes start`: the start `quickstart` makes without a plan or a terminal, and the ping it
/// prints. A running daemon is left alone, and one of another version is named so the operator
/// chooses when to restart it.
fn start_command(
    machine: bool,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let running = ping_once(env).is_ok();
    let answer = start(env, err)?;
    if running && !machine {
        writeln!(out, "The daemon is already running.").map_err(local)?;
    }
    render::ping(out, machine, &answer).map_err(local)?;
    if !machine {
        write_restart_nudge(out, &answer.version).map_err(local)?;
    }
    project_hint(env, machine, out)?;
    Ok(exit::SUCCESS)
}

fn project_hint(env: &Environment, machine: bool, out: &mut dyn Write) -> Result<(), Failure> {
    if !machine && no_projects(env) {
        writeln!(out, "{}", setup::NO_PROJECT_HINT).map_err(local)?;
    }
    Ok(())
}

fn no_projects(env: &Environment) -> bool {
    connect(env)
        .and_then(|mut client| reload(&mut client, env, None))
        .is_ok_and(|result| result.workspaces.is_empty())
}

/// The line that names a running daemon of another version and the way to replace it; nothing
/// when it runs this dudes' own version.
fn write_restart_nudge(to: &mut dyn Write, daemon: &str) -> std::io::Result<()> {
    if daemon == env!("CARGO_PKG_VERSION") {
        return Ok(());
    }
    writeln!(
        to,
        "The running daemon is {} and this dudes is {}. Run dudes stop, then dudes start, to use \
         the new version.",
        render::Escaped(daemon),
        env!("CARGO_PKG_VERSION")
    )
}

/// Start the daemon when none answers, and return its ping.
fn start(env: &Environment, err: &mut dyn Write) -> Result<PingResult, Failure> {
    crate::quickstart::run(env, || ping_once(env).ok(), err).map_err(local)
}

/// A scripted plan, its settings applied to `draft`. A start rule the result breaks is refused
/// when the draft is written, before anything is.
fn scripted(plan: Plan, draft: &mut Draft) -> Result<Plan, Failure> {
    for change in &plan.changes {
        draft.apply(change).map_err(local)?;
    }
    if let Some(hire) = &plan.hire
        && !setup::engine_on(draft.settings(), hire.provider)
    {
        return Err(local(format!(
            "nothing was written. {} is not on: pass --set providers.{} on",
            hire.provider, hire.provider
        )));
    }
    Ok(plan)
}

/// The guided plan: every question, then the summary the operator confirms.
fn guided(
    term: &mut Terminal<'_>,
    draft: &mut Draft,
    config: &Path,
    cwd: Option<&Path>,
    host: &setup::Host,
    running: bool,
) -> Result<Plan, Stopped> {
    let existing_setup = running || draft.settings().exists();
    let mut plan = setup::guided(term, draft, config, cwd, host, existing_setup)?;
    setup::summary(term, &plan, cwd)?;
    if running {
        term.say(
            "A daemon is running. When its settings differ from the file, quickstart restarts \
             it, which ends any turn it is running.",
        )?;
    }
    if !term.yes(&format!("Write {} and go ahead?", config.display()), true)? {
        return Err(Stopped::Declined);
    }
    plan.restart = true;
    Ok(plan)
}

/// The workspace entry `plan` enrolls at `cwd`, if it enrolls one.
fn enrollment(plan: &Plan, cwd: Option<&Path>) -> Result<Option<NewWorkspace>, Failure> {
    let (Some(enroll), Some(check)) = (&plan.enroll, &plan.check) else {
        return Ok(None);
    };
    let cwd = cwd.ok_or_else(|| local("the current directory cannot be read"))?;
    let name = cwd.file_name().map_or_else(
        || "workspace".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let id: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(128)
        .collect();
    let id = if id.is_empty() {
        "workspace".to_owned()
    } else {
        id
    };
    let mut label = name;
    while label.len() > 128 {
        label.pop();
    }
    Ok(Some(NewWorkspace {
        path: cwd.to_path_buf(),
        id,
        label,
        completion_check_label: enroll.check_label.clone(),
        department: enroll.department.clone(),
        agent_usd_per_day: enroll.agent_usd_per_day.clone(),
        goal_usd: enroll.goal_usd.clone(),
        hop_cap: check.hop_cap,
        predicate_argv: check.argv.clone(),
        predicate_timeout_s: check.timeout_s,
        tokens_per_day: None,
        goal_tokens: None,
    }))
}

/// What the running daemon runs, against the operator file this CLI reads.
enum Comparison {
    /// The daemon's answer would not describe this file as this CLI reads it, for this reason: it
    /// is another version, or it reads another file.
    Unknown(String),
    /// What it runs differently from this file, by `floor.enrollment.reload` and `system.doctor`.
    Known {
        /// The SHA-256 of the bytes it read, which `dudes workspace reload --apply` takes.
        sha256: String,
        /// Start-time settings it runs differently: each section the reload reports and each
        /// engine table `system.doctor` disagrees with. Each takes a restart.
        settings: Vec<String>,
        /// Workspace entries the file adds, removes or changes, which a reload adopts while it
        /// runs.
        workspaces: Vec<String>,
    },
}

enum ReloadCheck {
    Unknown(ReloadMismatch),
    Known(EnrollmentReloadResult),
}

const RESTART_COMMANDS: &str = "`dudes stop`, then `dudes start`";

enum ReloadMismatch {
    Version { running: String },
    File { path: Option<String> },
}

impl ReloadMismatch {
    fn reason(&self) -> String {
        match self {
            Self::Version { running } => format!(
                "the running daemon is {running}, this CLI is {}",
                env!("CARGO_PKG_VERSION")
            ),
            Self::File { path } => format!(
                "the running daemon reads {}, not this file",
                path.as_deref().unwrap_or("a file whose path is not UTF-8")
            ),
        }
    }

    fn with_advice(&self) -> String {
        let advice = match self {
            Self::Version { .. } => format!("restart it with {RESTART_COMMANDS}, then retry"),
            Self::File { .. } => {
                let variables = if cfg!(target_os = "macos") {
                    "HOME"
                } else if cfg!(windows) {
                    "APPDATA"
                } else {
                    "XDG_CONFIG_HOME (or its HOME fallback)"
                };
                format!(
                    "align this CLI's {variables} with the daemon's configuration path, then retry"
                )
            }
        };
        format!("{}; {advice}", self.reason())
    }
}

/// Compare the operator file with the running daemon. A daemon of another version may not compare
/// every setting this CLI writes, so its answer is not asked for. The reload parses the file as the
/// daemon does at start, so a file it would refuse is an error here, and an error is never a
/// difference.
fn checked_reload(env: &Environment, config: &Path) -> Result<ReloadCheck, Failure> {
    let version = ping_once(env)?.version;
    if version != env!("CARGO_PKG_VERSION") {
        return Ok(ReloadCheck::Unknown(ReloadMismatch::Version {
            running: version,
        }));
    }
    let check = reload(&mut connect(env)?, env, None)?;
    if !check
        .path
        .as_deref()
        .is_some_and(|read| same_file(Path::new(read), config))
    {
        return Ok(ReloadCheck::Unknown(ReloadMismatch::File {
            path: check.path,
        }));
    }
    Ok(ReloadCheck::Known(check))
}

fn running_differences(env: &Environment, config: &Path) -> Result<Comparison, Failure> {
    let check = match checked_reload(env, config)? {
        ReloadCheck::Unknown(mismatch) => return Ok(Comparison::Unknown(mismatch.reason())),
        ReloadCheck::Known(check) => check,
    };
    let workspaces = [(&check.added, "added"), (&check.removed, "removed")]
        .into_iter()
        .flat_map(|(ids, how)| ids.iter().map(move |id| format!("workspace {id} {how}")))
        .chain(
            check
                .changed
                .iter()
                .map(|entry| format!("workspace {} changed", entry.id)),
        )
        .collect();
    let mut settings = check.not_applied;
    for adapter in stale_adapters(env, config)? {
        if !settings.iter().any(|setting| setting == adapter) {
            settings.push(adapter.to_owned());
        }
    }
    Ok(Comparison::Known {
        sha256: check.sha256,
        settings,
        workspaces,
    })
}

/// Whether `read` and `config` name one file, as given or once canonicalized.
fn same_file(read: &Path, config: &Path) -> bool {
    read == config
        || matches!(
            (read.canonicalize(), config.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

/// The engine keys whose table the file and the running daemon's `system.doctor` disagree on.
fn stale_adapters(env: &Environment, config: &Path) -> Result<Vec<&'static str>, Failure> {
    let running = typed::<DoctorResult>(operator_call(env, Method::SystemDoctor, None)?)?.providers;
    let file = config::Settings::read(config).map_err(local)?.adapters();
    Ok(config::Key::ALL
        .into_iter()
        .filter(|key| {
            key.adapter().is_some_and(|provider| {
                file.contains(&provider) != running.iter().any(|row| row.provider == provider)
            })
        })
        .map(config::Key::name)
        .collect())
}

/// One operator call on a fresh connection, answered with its result value.
fn operator_call(
    env: &Environment,
    method: Method,
    params: Option<Value>,
) -> Result<Value, Failure> {
    let cred = operator_credential(env)?;
    answer_result(call_presenting(&mut connect(env)?, method, params, cred)?)
}

fn hard_stop_daemon(env: &Environment, err: &mut dyn Write) -> Result<(), Failure> {
    let shutdown = typed::<ShutdownResult>(operator_call(env, Method::SystemShutdown, None)?)?;
    if let Some(warning) = await_stopped(shutdown)? {
        render::warning(err, warning).map_err(local)?;
    }
    Ok(())
}

/// The Floor's one-time link, printed as `dudes floor open` prints it. A daemon that serves no
/// Floor is a warning, because the daemon itself is running.
fn floor_link(env: &Environment, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), Failure> {
    match operator_call(env, Method::FloorSessionIssue, None) {
        Ok(issued) => {
            let issued = typed::<SessionIssueResult>(issued)?;
            let urls = crate::floor::urls(&issued).map_err(local)?;
            render::floor_url(out, false, &issued, &urls).map_err(local)
        }
        Err(failure) => {
            render::warning(err, &format!("no Floor link: {}", failure.message())).map_err(local)
        }
    }
}

/// The name the daemon records for a workspace's own check, as quickstart enrolls it.
fn enrolled_check(label: &str) -> Option<CompletionCheckDeclaration> {
    CompletionCheckDeclaration::new(ENROLLED_CHECK_ID, label).ok()
}

/// The workspace a quickstart run enrolled with its first goal's check.
struct Enrolled<'a> {
    /// The workspace id.
    id: &'a str,
    /// The operator file it is in.
    config: &'a Path,
    /// The name the daemon records for the check.
    check: Option<CompletionCheckDeclaration>,
}

/// `agent.hire` in `cwd`, then `goal.set` when the hire has a goal. `enrolled` is the workspace
/// this run wrote with the same check.
fn hire_first(
    env: &Environment,
    hire: &setup::Hire,
    department: &str,
    check: Option<&setup::Check>,
    cwd: Option<&Path>,
    enrolled: Option<Enrolled<'_>>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let cwd = cwd.ok_or_else(|| local("the current directory cannot be read"))?;
    let (named, enrolled_at) = enrolled.map_or((None, None), |enrolled| {
        (enrolled.check, Some((enrolled.id, enrolled.config)))
    });
    let params = HireParams {
        name: hire.name.clone(),
        character: None,
        accent: None,
        role: hire.role.clone(),
        department: department.to_owned(),
        provider: hire.provider,
        cwd: cwd.to_path_buf(),
        budget: AgentBudget {
            usd_per_day: hire.usd_per_day.map(Usd::from_dollars),
            tokens_per_day: hire.tokens_per_day.map(Tokens::new),
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        checkout: None,
        from: None,
        mcp_servers: Vec::new(),
    };
    let hired = typed::<HireResult>(operator_call(
        env,
        Method::AgentHire,
        Some(encode(&params)?),
    )?)?;
    render::hired(out, false, &hired).map_err(local)?;
    let (Some(text), Some(check)) = (&hire.goal, check) else {
        return Ok(());
    };
    let goal = GoalSetParams {
        agent: hired.agent,
        text: text.clone(),
        predicate: Predicate::Command {
            argv: check.argv.clone(),
            timeout_s: check.timeout_s,
        },
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: check.hop_cap,
        check: named,
        may_already_pass: false,
    };
    let set = typed::<GoalSetResult>(
        operator_call(env, Method::GoalSet, Some(encode(&goal)?)).map_err(|failure| {
            let passes = failure.check_already_passes();
            failure.noted(&no_goal_note(
                &hire.name,
                hired.agent,
                check.hop_cap,
                enrolled_at,
                passes,
            ))
        })?,
    )?;
    render::goal_set(out, false, &set).map_err(local)
}

/// What quickstart leaves behind when the daemon refuses the first goal, and how to finish by hand.
/// `already_passes` says the daemon refused because the check already passes, which needs a
/// check that fails until the goal is met rather than one that can run.
fn no_goal_note(
    name: &str,
    agent: dudes_types::AgentId,
    hop_cap: u32,
    enrolled: Option<(&str, &Path)>,
    already_passes: bool,
) -> String {
    let command = format!(
        "dudes goal --agent {agent} --text <text> --hop-cap {hop_cap} -- <program> [args...]"
    );
    if already_passes {
        let hired = format!(
            "{name} is already hired, so running quickstart again would hire a second agent; give \
             {name} a goal whose check fails until the goal is met, with: {command}"
        );
        return match enrolled {
            None => hired,
            Some((id, _)) => format!(
                "{hired}; the Floor offers workspace {id}'s check, which already passes, so add \
                 one for the next goal there with dudes workspace check add {id} --id <id> \
                 --label <text> -- <program> [args...]"
            ),
        };
    }
    let hired = format!(
        "{name} is already hired, so running quickstart again would hire a second agent; give \
         {name} a goal with: {command}"
    );
    match enrolled {
        None => hired,
        Some((id, config)) => format!(
            "{hired}; workspace {id} is enrolled with the same check, so the Floor offers no hire \
             there until you change its predicate_argv in {} and run dudes workspace reload, then \
             dudes workspace reload --apply <sha256>",
            config.display()
        ),
    }
}

/// One `system.ping` through the same connection, credential and presentation path as `ping`.
fn ping_once(env: &Environment) -> Result<PingResult, Failure> {
    let mut client = connect(env)?;
    let cred = operator_credential(env)?;
    let response = call_presenting(&mut client, Method::SystemPing, None, cred)?;
    typed(answer_result(response)?)
}

/// Which method a command is, and its params as the contract's own type.
fn wire(command: &Command) -> Result<(Method, Option<Value>), Failure> {
    Ok(match command {
        Command::Ping => (Method::SystemPing, None),
        Command::Doctor | Command::Status => (Method::SystemDoctor, None),
        Command::StopHard => (Method::SystemShutdown, None),
        Command::Agents => (Method::AgentList, None),
        Command::Hire(params) => (Method::AgentHire, Some(encode(&**params)?)),
        Command::Rename(params) => (Method::AgentRename, Some(encode(params)?)),
        Command::Look(params) => (Method::AgentLook, Some(encode(params)?)),
        Command::Mcp(params) => (Method::AgentMcp, Some(encode(params)?)),
        Command::Goal(_) => unreachable!("dispatch sets a goal through set_goal"),
        Command::Pause(agent) => (
            Method::LoopPause,
            Some(encode(&AgentRef { agent: *agent })?),
        ),
        Command::Resume(agent) => (
            Method::LoopResume,
            Some(encode(&AgentRef { agent: *agent })?),
        ),
        Command::Halt(agent) => (Method::LoopHalt, Some(encode(&AgentRef { agent: *agent })?)),
        Command::Retire { agent, .. } => (
            Method::AgentRetire,
            Some(encode(&RetireParams {
                agent: *agent,
                reason: "retired by operator".to_owned(),
            })?),
        ),
        Command::Why { agent } => (Method::AgentWhy, Some(encode(&AgentRef { agent: *agent })?)),
        // The exact method, with the absent params the contract gives it, over the same
        // operator door as every row above: the credential is read once in [`dispatch`] and
        // presented once in [`call_presenting`]. The method's `Requirement` is the daemon's registration to
        // enforce, not this enum's to restate.
        Command::FloorOpen => (Method::FloorSessionIssue, None),
        Command::Board(BoardCommand::Create(params)) => {
            (Method::BoardCreate, Some(encode(params)?))
        }
        Command::Board(BoardCommand::List(params)) => (Method::BoardList, Some(encode(params)?)),
        Command::Board(BoardCommand::Claim {
            card,
            agent,
            expected_seq,
        }) => (
            Method::BoardClaim,
            Some(encode(&CardClaimParams {
                card: *card,
                agent: *agent,
                expected_seq: *expected_seq,
                token: None,
            })?),
        ),
        Command::Board(BoardCommand::Move {
            card,
            to,
            expected_seq,
        }) => (
            Method::BoardTransition,
            Some(encode(&CardTransitionParams {
                card: *card,
                to: *to,
                expected_seq: *expected_seq,
                token: None,
            })?),
        ),
        // Dispatched above. Reported rather than `unreachable!`, because a panic is a worse
        // diagnosis than a message and this binary has no state worth aborting to protect.
        Command::Quickstart(_)
        | Command::SelectHire(_)
        | Command::Start
        | Command::Login(_)
        | Command::PauseAll
        | Command::GoalReplace(_)
        | Command::Ask { .. }
        | Command::Stop
        | Command::ClosingAck { .. }
        | Command::Memory(_)
        | Command::Team
        | Command::Tail { .. }
        | Command::Workspace(_)
        | Command::Worktree(_)
        | Command::Config(_)
        | Command::Ticket(_)
        | Command::Mail(_) => {
            return Err(Failure::Other(Message::Local(
                "internal: a locally dispatched command reached the call path".to_owned(),
            )));
        }
    })
}

/// `dudes workspace`: edit the operator file, then have a running daemon adopt exactly that edit.
fn workspace_command(
    command: &WorkspaceCommand,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let local =
        |error: workspace::WorkspaceError| Failure::Other(Message::Local(error.to_string()));
    let config = operator_config(env)?;
    match command {
        WorkspaceCommand::Add(new) => {
            let new = new
                .resolve(
                    env.current_dir(),
                    &workspace::enrolled(config).map_err(local)?,
                )
                .map_err(local)?;
            let edit = workspace::add(config, &new).map_err(local)?;
            let status = adopt(
                env,
                config,
                &edit,
                EnrollmentChange {
                    added: vec![new.id.clone()],
                    ..EnrollmentChange::default()
                },
                machine,
                (out, err),
            )?;
            if status == exit::SUCCESS && !machine {
                writeln!(out, "Enrolled project {} at {} with check {}. To change it, run `dudes workspace remove {}` and add it again with your options.", render::Escaped(&new.id), render::Escaped(&new.path.display().to_string()), render::Escaped(&new.completion_check_label), render::Escaped(&new.id))
                    .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
            }
            Ok(status)
        }
        WorkspaceCommand::Remove { id } => {
            let edit = workspace::remove(config, id).map_err(local)?;
            adopt(
                env,
                config,
                &edit,
                EnrollmentChange {
                    removed: vec![id.clone()],
                    ..EnrollmentChange::default()
                },
                machine,
                (out, err),
            )
        }
        WorkspaceCommand::Reload { apply } => {
            let check = match checked_reload(env, config)? {
                ReloadCheck::Known(check) => check,
                ReloadCheck::Unknown(mismatch) => {
                    return Err(Failure::Other(Message::Local(mismatch.with_advice())));
                }
            };
            let result = if let Some(sha256) = apply {
                reload(
                    &mut connect(env)?,
                    env,
                    Some(EnrollmentApply {
                        sha256: sha256.clone(),
                        added: check.added,
                        removed: check.removed,
                        changed: check.changed.into_iter().map(|entry| entry.id).collect(),
                    }),
                )?
            } else {
                check
            };
            finish_reload(env, config, machine, out, &result)
        }
        WorkspaceCommand::CheckAdd(new) => {
            let edit = workspace::add_check(config, new).map_err(local)?;
            adopt(
                env,
                config,
                &edit,
                EnrollmentChange::changed(&new.workspace),
                machine,
                (out, err),
            )
        }
        WorkspaceCommand::CheckRemove { workspace, id } => {
            let edit = workspace::remove_check(config, workspace, id).map_err(local)?;
            adopt(
                env,
                config,
                &edit,
                EnrollmentChange::changed(workspace),
                machine,
                (out, err),
            )
        }
        WorkspaceCommand::List => {
            let file = workspace::enrolled(config).map_err(local)?;
            let running = match connect(env) {
                Ok(mut client) => Some(reload(&mut client, env, None)?.workspaces),
                Err(_) => None,
            };
            render::workspaces(out, machine, config, &file, running.as_deref())
                .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
            Ok(exit::SUCCESS)
        }
    }
}

/// The operator file both `workspace` and `config` edit.
fn operator_config(env: &Environment) -> Result<&std::path::Path, Failure> {
    env.config()
        .map_err(|error| Failure::Other(Message::Local(error.to_string())))
}

/// `dudes config`: show the file's start-time settings, or edit them and say whether a running
/// daemon needs a restart to read the edit, warning on stderr about a reserve taken to zero.
fn config_command(
    command: &ConfigCommand,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let config = operator_config(env)?;
    let local = |error: config::ConfigError| Failure::Other(Message::Local(error.to_string()));
    let written = match command {
        ConfigCommand::Show => {
            let settings = config::Settings::read(config).map_err(local)?;
            render::config(out, config, &settings)
        }
        ConfigCommand::Edit(changes) => {
            let edited = config::edit(config, changes).map_err(local)?;
            for warning in edited.warnings() {
                let _ = render::warning(err, &warning);
            }
            if !edited.wrote {
                writeln!(
                    out,
                    "{} already holds these settings; nothing was written",
                    config.display()
                )
            } else if connect(env).is_ok() {
                restart_advice(env, config, out)
            } else {
                writeln!(
                    out,
                    "wrote {}; the daemon reads it when it starts",
                    config.display()
                )
            }
        }
    };
    written.map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
    Ok(exit::SUCCESS)
}

/// What `config set` says after it wrote `config` while a daemon runs: the start-time settings a
/// restart would apply and the workspace changes a reload adopts without one, the reason the daemon
/// cannot be compared with this file, or its refusal of the file.
fn restart_advice(env: &Environment, config: &Path, out: &mut dyn Write) -> std::io::Result<()> {
    use render::Escaped;
    let written = config.display();
    let restart = RESTART_COMMANDS;
    let (sha256, settings, workspaces) = match running_differences(env, config) {
        Ok(Comparison::Known {
            sha256,
            settings,
            workspaces,
        }) => (sha256, settings, workspaces),
        Ok(Comparison::Unknown(reason)) => {
            return writeln!(
                out,
                "wrote {written}; {}, so restart it to apply these settings: {restart}",
                Escaped(&reason)
            );
        }
        Err(Failure::Other(Message::Rpc(error)))
            if error.code == dudes_proto::rpc::ErrorCode::InvalidParams.code() =>
        {
            return writeln!(
                out,
                "wrote {written}; the running daemon refuses the file: {}",
                Escaped(&error.message)
            );
        }
        Err(_) => {
            return writeln!(
                out,
                "wrote {written}; the running daemon reads these settings only when it starts, so \
                 restart it to apply them: {restart}"
            );
        }
    };
    if settings.is_empty() {
        writeln!(
            out,
            "wrote {written}; the running daemon already runs these settings, so no restart is \
             needed"
        )?;
    } else {
        writeln!(
            out,
            "wrote {written}; the running daemon reads other settings ({}) and reads the file only \
             when it starts, so restart it to apply them: {restart}",
            Escaped(&settings.join(", "))
        )?;
    }
    if !workspaces.is_empty() {
        writeln!(
            out,
            "the running daemon has not adopted the file's workspace changes ({}); `dudes \
             workspace reload --apply {}` adopts them without a restart",
            Escaped(&workspaces.join(", ")),
            Escaped(&sha256)
        )?;
    }
    Ok(())
}

/// Apply exactly this edit, or put the file back. An edit already in the file, or one slipped in
/// after the rename, is shown and never adopted.
///
/// The file goes back only when the daemon's answer proves nothing was applied. When the answer
/// never arrives, or says the daemon applied a file at another path, the edit stays and the
/// message says so, so the file never loses a change the daemon may be running.
/// The workspace ids one edit adds, removes or changes, which a reload must find exactly.
#[derive(Default)]
struct EnrollmentChange {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

impl EnrollmentChange {
    fn changed(id: &str) -> Self {
        Self {
            changed: vec![id.to_owned()],
            ..Self::default()
        }
    }
}

fn adopt(
    env: &Environment,
    config: &std::path::Path,
    edit: &Edit,
    change: EnrollmentChange,
    machine: bool,
    (out, err): (&mut dyn Write, &mut dyn Write),
) -> Result<u8, Failure> {
    let adding = change.added.len() == 1 && change.removed.is_empty() && change.changed.is_empty();
    let Ok(mut client) = connect(env) else {
        let _ = writeln!(
            out,
            "wrote {}; no daemon is running, and the daemon reads it when it starts",
            config.display()
        );
        return Ok(exit::SUCCESS);
    };
    let restore = || {
        workspace::restore(config, edit.previous.as_deref())
            .map_err(|error| Failure::Other(Message::Local(error.to_string())))
    };
    let cred = match operator_credential(env) {
        Ok(cred) => cred,
        Err(failure) => {
            restore()?;
            return Err(failure);
        }
    };
    let apply = EnrollmentApply {
        sha256: edit.sha256(),
        added: change.added,
        removed: change.removed,
        changed: change.changed,
    };
    let result = match reload_presenting(&mut client, cred, Some(apply)) {
        Ok(result) => result,
        Err(failure @ (Failure::Refused(_) | Failure::Other(Message::Rpc(_)))) => {
            restore()?;
            return Err(failure);
        }
        Err(failure) => {
            let unknown = |message| {
                format!(
                    "{message}; {} keeps this edit, and whether the daemon adopted it is \
                     unknown: `dudes workspace reload` shows what differs",
                    config.display()
                )
            };
            return Err(match failure {
                Failure::Unreachable(message) => Failure::Unreachable(unknown(message)),
                Failure::Other(Message::Local(message)) => {
                    Failure::Other(Message::Local(unknown(message)))
                }
                answered => answered,
            });
        }
    };
    if !result.applied {
        restore()?;
    }
    if result.path.as_deref() != config.to_str() {
        let read = result.path.as_deref().unwrap_or("a path that is not UTF-8");
        if !result.applied {
            return Err(Failure::Other(Message::Local(format!(
                "the daemon read {read}, not {}; the file was put back",
                config.display()
            ))));
        }
        let _ = render::warning(
            err,
            &format!(
                "the daemon read {read}, not {}, and applied that file's change; this file keeps \
                 the edit",
                config.display()
            ),
        );
    }
    if adding && !machine && result.applied {
        if !result.not_applied.is_empty() {
            writeln!(
                out,
                "needs a daemon restart: {}",
                render::Escaped(&result.not_applied.join(", "))
            )
            .map_err(local)?;
        }
        Ok(exit::SUCCESS)
    } else {
        finish(out, machine, &result)
    }
}

/// One `floor.enrollment.reload` call over the operator door.
fn reload(
    client: &mut Socket,
    env: &Environment,
    apply: Option<EnrollmentApply>,
) -> Result<EnrollmentReloadResult, Failure> {
    let cred = operator_credential(env)?;
    reload_presenting(client, cred, apply)
}

/// [`reload`] with the credential already read.
fn reload_presenting(
    client: &mut Socket,
    cred: Option<dudes_proto::rpc::cred::OperatorCredential>,
    apply: Option<EnrollmentApply>,
) -> Result<EnrollmentReloadResult, Failure> {
    let params = encode(&EnrollmentReloadParams { apply })?;
    let response = call_presenting(client, Method::FloorEnrollmentReload, Some(params), cred)?;
    typed(answer_result(response)?)
}

/// `workspace reload`'s result. The human form also names each engine table whose presence differs
/// from the adapters `system.doctor` lists.
fn finish_reload(
    env: &Environment,
    config: &std::path::Path,
    machine: bool,
    out: &mut dyn Write,
    result: &EnrollmentReloadResult,
) -> Result<u8, Failure> {
    let code = finish(out, machine, result)?;
    if machine {
        return Ok(code);
    }
    let adapters: Vec<_> = stale_adapters(env, config)?
        .into_iter()
        .filter(|adapter| !result.not_applied.iter().any(|section| section == adapter))
        .collect();
    if !adapters.is_empty() {
        writeln!(out, "needs a daemon restart: {}", adapters.join(", "))
            .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
    }
    Ok(code)
}

/// Render a reload's result; a declined `apply` is the daemon declining the call.
fn finish(
    out: &mut dyn Write,
    machine: bool,
    result: &EnrollmentReloadResult,
) -> Result<u8, Failure> {
    render::reload(out, machine, result)
        .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
    Ok(if result.refused.is_some() {
        exit::REFUSED
    } else {
        exit::SUCCESS
    })
}

/// A params value as JSON.
///
/// Serialisation cannot fail for any of these — they are plain structs of owned values — but it is
/// reported rather than unwrapped, for the reason above.
fn encode<T: serde::Serialize>(value: &T) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(|error| Failure::Other(Message::Local(error.to_string())))
}

/// Sort the answer, render it, and give the exit code.
fn render_answer(
    invocation: &Invocation,
    method: Method,
    response: Response,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let result = answer_result(response)?;

    // Deserialised into the contract's own type before anything is printed, so a `--json` run that
    // succeeds is *also* the assertion that the answer was that type. Echoing the daemon's bytes
    // would print the same characters and prove nothing.
    let machine = invocation.json;
    let rendered = match method {
        Method::SystemPing => render::ping(out, machine, &typed::<PingResult>(result)?),
        Method::SystemShutdown => render::stopped(out, machine, &typed::<ShutdownResult>(result)?),
        Method::AgentList => {
            render::agents(out, machine, &typed::<Vec<dudes_proto::AgentView>>(result)?)
        }
        Method::TeamRoster => render::team(
            out,
            machine,
            &typed::<Vec<dudes_proto::rpc::team::RosterEntry>>(result)?,
        ),
        Method::AgentHire => render::hired(out, machine, &typed::<HireResult>(result)?),
        Method::GoalSet => render::goal_set(out, machine, &typed::<GoalSetResult>(result)?),
        Method::LoopPause | Method::LoopResume => render::loop_control(
            out,
            machine,
            method == Method::LoopResume,
            typed::<Empty>(result)?,
        ),
        Method::LoopHalt => render::halted(out, machine, typed::<Empty>(result)?),
        Method::AgentRetire => render::retired(out, machine, typed::<Empty>(result)?),
        Method::AgentRename => render::renamed(out, machine, typed::<Empty>(result)?),
        Method::AgentLook => render::look_changed(out, machine, typed::<Empty>(result)?),
        Method::AgentMcp => render::mcp_servers(out, machine, &typed::<McpResult>(result)?),
        Method::AgentAsk => render::asked(out, machine, typed::<Empty>(result)?),
        Method::MemoryAppend => render::memory_appended(
            out,
            machine,
            typed::<dudes_proto::rpc::memory::MemoryAppendResult>(result)?,
        ),
        Method::MemoryRead => render::memory_read(
            out,
            machine,
            &typed::<dudes_proto::rpc::memory::MemoryReadResult>(result)?,
        ),
        Method::MemorySearch => render::memory_search(
            out,
            machine,
            &typed::<dudes_proto::rpc::memory::MemorySearchResult>(result)?,
        ),
        Method::AgentWhy => render::why(out, machine, &typed::<WhyResult>(result)?),
        Method::BoardCreate => {
            render::card_created(out, machine, &typed::<CardCreateResult>(result)?)
        }
        Method::BoardList => {
            render::cards(out, machine, &typed::<Vec<dudes_proto::CardView>>(result)?)
        }
        Method::SystemDoctor => {
            let journal = typed::<dudes_proto::rpc::system::DoctorJournalResult>(result.clone())?;
            let mailbox =
                typed::<dudes_proto::rpc::system::DoctorMailboxResult>(result.clone())?.mailbox;
            if machine {
                render::doctor_json(out, &result)
            } else {
                render::doctor(out, false, &journal).map_err(local)?;
                render::mailbox_report(out, &mailbox)
            }
        }
        Method::BoardClaim | Method::BoardTransition => {
            render::card_updated(out, machine, typed::<Empty>(result)?)
        }
        // The one renderer that can refuse its own answer. Invariant 10 admits no listener outside
        // loopback, so an origin that is not one is a daemon this CLI will not vouch for: the URL
        // is not composed, nothing is printed, and the exit code says the run failed. Composing
        // first and printing second is deliberate — a refusal after a print would have disclosed
        // the bearer for a Floor it then declined.
        Method::FloorSessionIssue => {
            let issued = typed::<SessionIssueResult>(result)?;
            let urls = crate::floor::urls(&issued)
                .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
            render::floor_url(out, machine, &issued, &urls)
        }
        other => {
            return Err(Failure::Other(Message::Local(format!(
                "this binary has no renderer for {}",
                other.name()
            ))));
        }
    };
    rendered.map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
    Ok(exit::SUCCESS)
}

fn answer_result(response: Response) -> Result<Value, Failure> {
    match answered(response) {
        Answered::Ok(result) => Ok(result),
        Answered::Refused(error) => Err(Failure::Refused(error)),
        Answered::Failed(error) => Err(Failure::Other(Message::Rpc(Box::new(error)))),
    }
}

/// A result value as the contract's type, or a failure naming what did not fit.
fn typed<T: DeserializeOwned>(result: Value) -> Result<T, Failure> {
    serde_json::from_value(result).map_err(|error| {
        Failure::Other(Message::Local(format!(
            "the daemon's answer is not this method's result type: {error}"
        )))
    })
}

/// `journal.tail`: subscribe, then read frames until the answer arrives — or for ever.
///
/// The response to `journal.tail` arrives **after** the backfill it asked for
/// (`dudes_rpc::methods::journal::tail`), so this reads frames and sorts them rather than reading
/// one answer. That is what makes `dudes tail --from 0` terminate on its own: the response is the
/// end of the backfill, and it is bounded by the journal.
///
/// With `--follow` the response is not the end — it is the point after which everything arriving is
/// live — so the loop keeps reading and this call does not return until the daemon closes the
/// connection.
fn tail(
    client: &mut Socket,
    invocation: &Invocation,
    cred: Option<dudes_proto::rpc::cred::OperatorCredential>,
    params: &dudes_proto::rpc::journal::JournalTailParams,
    follow: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let params = serde_json::to_value(params)
        .map_err(|error| Failure::Other(Message::Local(error.to_string())))?;
    let id = client.send_presenting(Method::JournalTail, Some(params), cred)?;

    let mut subscribed = false;
    loop {
        match client.recv() {
            Ok(Inbound::Notification(notification)) => {
                render_notification(invocation, &notification, out, err)?;
            }
            Ok(Inbound::Response(response)) => {
                if response.id != id {
                    // Nothing in this binary sends a second call on one connection, so an id that
                    // is not ours is a daemon that answered something nobody asked. Reported rather
                    // than ignored: a silent drop here would hide a real transport defect.
                    return Err(Failure::Other(Message::Local(format!(
                        "the daemon answered an id this call did not send: {:?}",
                        response.id
                    ))));
                }
                match answered(response) {
                    Answered::Ok(_) => subscribed = true,
                    Answered::Refused(error) => return Err(Failure::Refused(error)),
                    Answered::Failed(error) => {
                        return Err(Failure::Other(Message::Rpc(Box::new(error))));
                    }
                }
                if !follow {
                    return Ok(exit::SUCCESS);
                }
            }
            // A closed connection ends a `--follow` that was serving, and is a failure otherwise.
            // The distinction is the whole reason `subscribed` is tracked: a daemon that closed
            // before answering never accepted the subscription, and reporting that as success would
            // make `dudes tail` silently print nothing when the door was shut in its face.
            Err(ClientError::Closed) if subscribed => return Ok(exit::SUCCESS),
            Err(error) => return Err(Failure::from(error)),
        }
    }
}

fn render_notification(
    invocation: &Invocation,
    notification: &dudes_proto::rpc::Notification,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    if notification.method != NotificationKind::JournalAppended.name() {
        // A notification this binary does not render is noted and skipped, not fatal: the contract
        // grows notifications, and a CLI that died on an unknown one would break on a daemon
        // newer than itself. `#[serde(default)]` everywhere on the payloads is the same rule.
        let _ = render::local_error(
            err,
            &format!("ignoring an unknown notification `{}`", notification.method),
        );
        return Ok(());
    }
    let params: JournalAppendedParams = serde_json::from_value(
        notification.params.clone().unwrap_or(Value::Null),
    )
    .map_err(|error| {
        Failure::Other(Message::Local(format!(
            "a `journal.appended` did not carry this contract's params: {error}"
        )))
    })?;
    render::appended(out, invocation.json, &params, &chrono::Local)
        .map_err(|error| Failure::Other(Message::Local(error.to_string())))
}

/// Connect, or say the daemon is not there.
///
/// The message names the path, because "connection refused" with no path is the least useful thing
/// a CLI can say — and because `tests/token.rs` uses exactly this line as its positive control:
/// the socket path is an environment-derived value that *does* reach the output, which is what
/// proves the same capture would have caught a token that leaked.
fn connect(env: &Environment) -> Result<Socket, Failure> {
    let socket = env
        .socket()
        .map_err(|missing| Failure::Other(Message::Local(missing.to_string())))?;
    connect_at(socket)
}

fn connect_at(socket: &Path) -> Result<Socket, Failure> {
    LocalClient::connect(socket).map_err(|error| {
        Failure::Unreachable(format!(
            "cannot reach the daemon at {}: {error}",
            socket.display()
        ))
    })
}

/// The operator credential from the runtime directory: the read half of the operator door.
fn operator_credential(
    env: &Environment,
) -> Result<Option<dudes_proto::rpc::cred::OperatorCredential>, Failure> {
    let local = |message: String| Failure::Other(Message::Local(message));
    let runtime_dir = env
        .runtime_dir()
        .map_err(|missing| local(missing.to_string()))?;
    read_operator_credential(runtime_dir).map_err(|error| local(error.to_string()))
}

#[cfg(test)]
mod pause_all_tests {
    use super::*;
    use dudes_proto::AgentView;
    use dudes_proto::rpc::ErrorCode;
    use dudes_types::AgentId;
    use serde_json::json;

    fn row(id: &str, name: &str, status: &str, paused: bool, halted: bool) -> AgentView {
        serde_json::from_value(json!({
            "id": id, "name": name, "role": "engineer", "department": "engineering",
            "provider": "claude", "status": status, "mode": "managed", "paused": paused,
            "halted": halted,
        }))
        .expect("one projected agent")
    }

    fn rpc_error(code: ErrorCode, message: &str) -> RpcError {
        RpcError {
            code: code.code(),
            message: message.to_owned(),
            data: None,
        }
    }

    #[test]
    fn pause_each_continues_after_one_refusal_and_skips_existing_holds() {
        let rows = [
            row("01JBQ8XK2M3N4P5Q6R7S8T9VWX", "Ada", "running", false, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAV", "Bob", "waiting", false, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAW", "Cy", "attached", false, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAX", "Dee", "idle", true, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAY", "Eve", "running", false, true),
        ];
        let mut called = Vec::<AgentId>::new();
        let report = pause_each(&rows, |agent| {
            called.push(agent);
            if agent == rows[1].id {
                Err(Failure::Refused(rpc_error(
                    ErrorCode::Refused,
                    "admission refused",
                )))
            } else {
                Ok(())
            }
        });
        assert_eq!(called, [rows[0].id, rows[1].id, rows[2].id]);
        assert_eq!(report.paused, 2);
        assert_eq!(
            report.refusals,
            [("Bob".to_owned(), "admission refused".to_owned())]
        );
    }

    #[test]
    fn pause_each_does_not_call_an_internal_error_a_refusal() {
        let rows = [row(
            "01JBQ8XK2M3N4P5Q6R7S8T9VWX",
            "Ada",
            "running",
            false,
            false,
        )];
        let result = pause_each(&rows, |_| {
            Err(Failure::Other(Message::Rpc(Box::new(rpc_error(
                ErrorCode::InternalError,
                "journal failed",
            )))))
        });
        assert!(matches!(
            result.failure,
            Some(Failure::Other(Message::Rpc(_)))
        ));
    }

    #[test]
    fn pause_each_keeps_an_earlier_refusal_after_a_later_transport_failure() {
        let rows = [
            row("01JBQ8XK2M3N4P5Q6R7S8T9VWX", "Ada", "running", false, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAV", "Bob", "waiting", false, false),
            row("01ARZ3NDEKTSV4RRFFQ69G5FAW", "Cy", "attached", false, false),
        ];
        let report = pause_each(&rows, |agent| {
            if agent == rows[1].id {
                Err(Failure::Refused(rpc_error(
                    ErrorCode::Refused,
                    "admission refused",
                )))
            } else if agent == rows[2].id {
                Err(Failure::Unreachable("socket closed".to_owned()))
            } else {
                Ok(())
            }
        });
        assert_eq!(report.paused, 1);
        assert_eq!(
            report.refusals,
            [("Bob".to_owned(), "admission refused".to_owned())]
        );
        assert!(matches!(report.failure, Some(Failure::Unreachable(_))));
    }
}

#[cfg(test)]
mod closing_report_tests {
    use super::*;
    use dudes_proto::rpc::closing::ClosingReportRef;
    use dudes_proto::rpc::system::ShutdownReceipt;
    use dudes_types::{AgentId, Lsn, Timestamp};

    fn started(dir: &Path) -> ClosingStarted {
        ClosingStarted {
            announcement: Lsn(7),
            deadline: Timestamp::from_unix_millis(0),
            participants: vec![AgentId::NIL, "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap()],
            completion: ShutdownReceipt {
                path: dir.join("completion"),
                run: "run".into(),
            },
            report: ClosingReportRef {
                path: dir.join("report"),
                run: "run".into(),
            },
        }
    }

    fn report(started: &ClosingStarted) -> ClosingReport {
        ClosingReport {
            run: "run".into(),
            announcement: Lsn(7),
            complete: Lsn(9),
            participants: started.participants.clone(),
            acknowledged: vec![AgentId::NIL],
            missing: vec![started.participants[1]],
            settled: vec![],
        }
    }

    #[test]
    fn retries_keep_the_original_wall_deadline_and_call_budget() {
        let initial = Instant::now();
        let announced = Timestamp::from_unix_millis(360_000);
        assert_eq!(
            closing_receipt_deadline(initial, initial, 0, announced, CLOSING_STOP_DEADLINE),
            initial + Duration::from_secs(390)
        );
        let retry = initial + Duration::from_secs(200);
        assert_eq!(
            closing_receipt_deadline(retry, retry, 200_000, announced, CLOSING_STOP_DEADLINE),
            initial + Duration::from_secs(390)
        );
        let expired = initial + Duration::from_secs(450);
        assert_eq!(
            closing_receipt_deadline(expired, expired, 450_000, announced, CLOSING_STOP_DEADLINE),
            expired
        );
        let delayed = initial + Duration::from_secs(200);
        assert_eq!(
            closing_receipt_deadline(initial, delayed, 0, announced, CLOSING_STOP_DEADLINE),
            initial + Duration::from_secs(390)
        );
    }

    #[test]
    fn final_census_requires_the_exact_snapshot_and_partition() {
        let dir = tempfile::tempdir().unwrap();
        let start = started(dir.path());
        let valid = report(&start);
        validate_closing_report(&start, &valid).unwrap();
        let mut cases = Vec::new();
        let mut wrong = valid.clone();
        wrong.run = "another run".into();
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.announcement = Lsn(6);
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.complete = Lsn(7);
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.participants.reverse();
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.acknowledged.push(AgentId::NIL);
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.missing.push(start.participants[1]);
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.missing.clear();
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.acknowledged.push(start.participants[1]);
        cases.push(wrong);
        let mut wrong = valid.clone();
        wrong.missing = vec!["01ARZ3NDEKTSV4RRFFQ69G5FAW".parse().unwrap()];
        cases.push(wrong);
        for wrong in cases {
            assert!(
                validate_closing_report(&start, &wrong).is_err(),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn closing_report_reads_are_bounded_and_errors_do_not_echo_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let start = started(dir.path());
        std::fs::write(&start.completion.path, "complete run\n").unwrap();
        assert!(await_closing(&start, Instant::now(), Duration::from_secs(1)).is_err());
        let secret = "a report body must never be reflected";
        std::fs::write(
            &start.report.path,
            format!("{{\"announcement\":\"{secret}\"}}"),
        )
        .unwrap();
        let error = await_closing(&start, Instant::now(), Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.message(), "invalid closing report");
        assert!(!error.message().contains(secret));
        std::fs::write(&start.report.path, vec![b'x'; CLOSING_REPORT_MAX_BYTES + 1]).unwrap();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::from_secs(1))
                .unwrap_err()
                .message(),
            "the closing report exceeds its byte bound"
        );
        std::fs::write(
            &start.report.path,
            serde_json::to_vec(&report(&start)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::from_secs(1))
                .unwrap()
                .0,
            report(&start)
        );
        let mut exact_bound = serde_json::to_vec(&report(&start)).unwrap();
        exact_bound.resize(CLOSING_REPORT_MAX_BYTES, b' ');
        std::fs::write(&start.report.path, exact_bound).unwrap();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::from_secs(1))
                .unwrap()
                .0,
            report(&start)
        );
    }

    #[test]
    fn invalid_initial_censuses_and_report_locations_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut start = started(dir.path());
        start.participants.push(AgentId::NIL);
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
        start = started(dir.path());
        start.participants = (0..=CLOSING_PARTICIPANT_LIMIT)
            .map(|index| AgentId::from_parts(1, u128::try_from(index).unwrap()))
            .collect();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
        start = started(dir.path());
        start.report.path = dir.path().join("elsewhere").join("report");
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
        start = started(dir.path());
        start.report.run = "other".into();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
        start = started(dir.path());
        start.announcement = Lsn(0);
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
        start = started(dir.path());
        start.report.path = PathBuf::from("report");
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::ZERO)
                .unwrap_err()
                .message(),
            "the daemon returned an invalid closing announcement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unresponsive_announcement_returns_at_the_private_bound_without_joining() {
        use std::io::{BufRead as _, BufReader};
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("duded.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let (seen, received) = std::sync::mpsc::channel();
        let fixture = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "system.closingTime");
            assert_eq!(request["params"], serde_json::json!({"action":"announce"}));
            seen.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(2));
            drop(stream);
        });
        let called_at = Instant::now();
        let result = closing_stop(
            socket,
            dir.path().to_path_buf(),
            called_at,
            Duration::from_millis(100),
        );
        let elapsed = called_at.elapsed();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        fixture.join().unwrap();
        assert_eq!(
            result.unwrap_err().message(),
            "closing time did not finish within the original call deadline"
        );
        assert!(elapsed >= Duration::from_millis(100));
        assert!(elapsed < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn closing_report_refuses_symlinks_and_directories() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let start = started(dir.path());
        std::fs::write(&start.completion.path, "complete run\n").unwrap();
        std::fs::create_dir(&start.report.path).unwrap();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::from_secs(1))
                .unwrap_err()
                .message(),
            "the closing report is not a regular file"
        );
        std::fs::remove_dir(&start.report.path).unwrap();
        symlink(&start.completion.path, &start.report.path).unwrap();
        assert_eq!(
            await_closing(&start, Instant::now(), Duration::from_secs(1))
                .unwrap_err()
                .message(),
            "the closing report is not a regular file"
        );
    }
}

#[cfg(test)]
mod no_goal_note_tests {
    use super::*;

    #[test]
    fn the_note_names_the_hired_agent_a_full_goal_command_and_an_enrolled_check() {
        let agent: dudes_types::AgentId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        assert_eq!(
            no_goal_note("Kim", agent, 5, None, false),
            "Kim is already hired, so running quickstart again would hire a second agent; give \
             Kim a goal with: dudes goal --agent 01ARZ3NDEKTSV4RRFFQ69G5FAV --text <text> \
             --hop-cap 5 -- <program> [args...]"
        );
        assert_eq!(
            no_goal_note(
                "Kim",
                agent,
                5,
                Some(("repo", Path::new("/home/operator/dudes.toml"))),
                false
            ),
            "Kim is already hired, so running quickstart again would hire a second agent; give \
             Kim a goal with: dudes goal --agent 01ARZ3NDEKTSV4RRFFQ69G5FAV --text <text> \
             --hop-cap 5 -- <program> [args...]; workspace repo is enrolled with the same check, \
             so the Floor offers no hire there until you change its predicate_argv in \
             /home/operator/dudes.toml and run dudes workspace reload, then dudes workspace reload \
             --apply <sha256>"
        );
    }

    /// A first goal refused because its check already passes asks for a check that fails until
    /// the goal is met, and for another enrolled check, not for a check that can run.
    #[test]
    fn a_check_that_already_passes_asks_for_one_that_fails_first() {
        let agent: dudes_types::AgentId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        assert_eq!(
            no_goal_note(
                "Kim",
                agent,
                5,
                Some(("repo", Path::new("/home/operator/dudes.toml"))),
                true
            ),
            "Kim is already hired, so running quickstart again would hire a second agent; give \
             Kim a goal whose check fails until the goal is met, with: dudes goal --agent \
             01ARZ3NDEKTSV4RRFFQ69G5FAV --text <text> --hop-cap 5 -- <program> [args...]; the \
             Floor offers workspace repo's check, which already passes, so add one for the next \
             goal there with dudes workspace check add repo --id <id> --label <text> -- <program> \
             [args...]"
        );
    }
}

#[cfg(test)]
mod replacement_tests {
    use super::*;

    #[test]
    fn request_file_is_bounded_and_the_parsed_request_survives_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("request.json");
        let source = serde_json::json!({"request_id":"0".repeat(64),"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","text":" reviewed ","predicate":{"command":{"argv":["true","literal; $(text)"],"timeout_s":600}},"check":{"id":"tests","label":"Tests"}});
        let mut bytes = serde_json::to_vec(&source).unwrap();
        bytes.resize(131_072, b' ');
        std::fs::write(&file, &bytes).unwrap();
        let parsed = read_goal_replacement(&file).unwrap();
        assert_eq!(parsed.text, "reviewed");
        bytes.push(b' ');
        std::fs::write(&file, &bytes).unwrap();
        assert!(read_goal_replacement(&file).is_err());
        assert_eq!(
            parsed.predicate,
            Predicate::Command {
                argv: vec!["true".into(), "literal; $(text)".into()],
                timeout_s: 600
            }
        );
    }
}
