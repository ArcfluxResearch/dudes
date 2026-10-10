//! Parsing `Vec<OsString>` into a [`Command`], by hand.
//!
//! # No argument-parsing crate
//!
//! Commands and typed flags. `bins/duded/src/main.rs` states the house line — *"a dependency
//! is forever"* — no arg crate is in `[workspace.dependencies]` today, and adding one would change
//! the `cargo deny` surface for a saving this size. If the surface grows past what this file can be
//! read in one sitting, adding one is a decision to take then and to record.
//!
//! # Values arrive as `OsString` and are converted once, here
//!
//! Invariant 7's neighbour. Nothing in this binary builds a command string, so there is no shell to
//! be careful about; what there *is* to be careful about is lossy conversion. A value that is not
//! valid UTF-8 is **refused** rather than replacement-charactered, because the wire is JSON and JSON
//! is UTF-8: a goal whose text was silently mangled on the way in would be journaled mangled, and
//! the mangling would be indistinguishable from what the operator typed.
//!
//! `--cwd` is the exception and takes its `OsString` whole into a [`PathBuf`], because a path is not
//! text and the contract's field is a `PathBuf` too.
//!
//! # What cannot travel, stated rather than pretended about
//!
//! A NUL byte cannot appear in any argument of any process on Unix — `execve` takes NUL-terminated
//! strings — so "the CLI passes a NUL through unchanged" is not a property this or any argv-driven
//! program can have. Every other byte a shell treats as special is ordinary here, and
//! `tests/metacharacters.rs` asserts the round trip for them.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use dudes_proto::budget::{AgentBudget, GoalBudget};
use dudes_proto::goal::Predicate;
use dudes_proto::rpc::agent::{HireParams, LookParams, McpParams, RenameParams};
use dudes_proto::rpc::board::{CardCreateParams, CardListParams};
use dudes_proto::rpc::goal::{CompletionCheckDeclaration, GoalSetParams};
use dudes_proto::rpc::journal::JournalTailParams;
use dudes_types::{AgentId, CardId, CardStatus, Lsn, MailId, Provider, Seq, Tokens, Usd};

use crate::config::{self, Change, Key};
use crate::setup::{self, Check, Enroll, Hire, Plan};
use crate::workspace::NewWorkspace;

/// What the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Render the contract's own types rather than a human summary.
    pub json: bool,
    /// The command.
    pub command: Command,
}

/// One command this binary carries.
///
/// Operator commands call rows, `workspace` after its local file edit; `ask` is
/// `Agent`; `ping` admits every role; `quickstart` is local startup followed by that same ping. Which role each RPC admits belongs to the
/// daemon's registration of that method, not to this enum: a CLI that carried its own copy of
/// each method's `Requirement` would be a second table to keep in step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start the daemon when absent, then complete `system.ping`. With a [`Plan`], set up the
    /// operator file first, then report the engines, print the Floor link and make the hire.
    /// Without one, a terminal is asked for the plan and anything else only starts the daemon.
    Quickstart(Option<Box<Plan>>),
    /// Start the daemon when absent, then complete `system.ping`. It asks nothing, writes no
    /// configuration and hires nobody.
    Start,
    /// `system.ping`.
    Ping,
    /// `system.doctor` diagnostics.
    Doctor,
    /// `system.doctor` current daemon status.
    Status,
    /// `system.closingTime`, then wait for completion and the final acknowledgement census.
    Stop,
    /// `system.shutdown`, preserving the immediate stop and its completion receipt.
    StopHard,
    /// Acknowledge this agent's own participation using its inherited token.
    ClosingAck {
        /// The immutable announcement journal position.
        announcement_lsn: Lsn,
    },
    /// `agent.list`.
    Agents,
    /// `team.roster`, using the inherited agent token or operator credential.
    Team,
    /// `agent.list`, then one `loop.pause` for each current agent without a standing pause or halt.
    PauseAll,
    /// Hold future issuance for one agent.
    Pause(AgentId),
    /// Clear pause, halt and automatic hold for one agent.
    Resume(AgentId),
    /// Record a halt for one agent and request owned-child teardown.
    Halt(AgentId),
    /// Retire one agent after local confirmation.
    Retire {
        /// The agent.
        agent: AgentId,
        /// Explicit consent for noninteractive or machine-readable use.
        yes: bool,
    },
    /// `agent.hire`.
    Hire(Box<HireParams>),
    /// `goal.set`.
    Goal(Box<GoalSetParams>),
    /// Submit one locally reviewed replacement request file.
    GoalReplace(PathBuf),
    /// `agent.ask`. The token comes from the process environment; the daemon derives the agent.
    Ask {
        /// The question.
        text: String,
    },
    /// Agent memory, or an explicitly named operator target.
    Memory(MemoryCommand),
    /// Offline operations on the mailbox named by `DUDES_MAILBOX`.
    Mail(MailCommand),
    /// `agent.why`.
    Why {
        /// Which agent.
        agent: AgentId,
    },
    /// `journal.tail`.
    Tail {
        /// Where to start.
        params: JournalTailParams,
        /// Keep reading after the backfill instead of exiting when the response arrives.
        follow: bool,
    },
    /// `floor.session.issue`, printed as the one URL to paste into a browser.
    FloorOpen,
    /// Run an engine's own sign-in from the path the daemon found.
    Login(Provider),
    /// `workspace add|remove|reload|list`: edit `[floor.onboarding]` and have the daemon adopt
    /// the change with `floor.enrollment.reload`.
    Workspace(WorkspaceCommand),
    /// Operator board commands.
    Board(BoardCommand),
    /// `agent.rename`.
    Rename(RenameParams),
    /// `agent.look`.
    Look(LookParams),
    /// Read or replace desired servers for the next managed launch.
    Mcp(McpParams),
    /// `worktree list|remove`: the git worktrees the daemon created for agents.
    Worktree(WorktreeCommand),
    /// `config`: show or edit the operator file's start-time settings. Local; nothing is sent.
    Config(ConfigCommand),
}

/// One `config` subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
    /// Every key's value as the daemon will read it at its next start.
    Show,
    /// Set or remove keys, in one write.
    Edit(Vec<Change>),
}

/// One memory operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCommand {
    /// Append exact text.
    Append {
        /// Explicit operator target; absent for the agent's own memory.
        agent: Option<AgentId>,
        /// The text.
        text: String,
    },
    /// Read a bounded page of whole entries.
    Read {
        /// Explicit operator target; absent for the agent's own memory.
        agent: Option<AgentId>,
        /// Exclusive append cursor.
        since_lsn: Option<Lsn>,
    },
    /// Search original notes and the current summary with validated bounded inputs.
    Search {
        /// Explicit operator target; absent for the agent's own memory.
        agent: Option<AgentId>,
        /// Literal query, exclusive candidate cursor and hit limit.
        options: dudes_proto::rpc::memory::MemorySearchOptions,
    },
}

/// One offline mailbox operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailCommand {
    /// Publish an outgoing request for the daemon to ingest.
    Send(MailSend),
    /// Read the current inbox with validated bodies.
    Read,
    /// Move one delivered marker into the existing acknowledgement directory.
    Done(MailId),
}

/// A parsed outgoing request with exactly one route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailSend {
    pub(crate) route: MailRoute,
    pub(crate) act: dudes_proto::MailAct,
    pub(crate) subject: String,
    pub(crate) body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MailRoute {
    To(MailAddress),
    Reply(MailId),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum MailAddress {
    Agent { agent: AgentId },
    Agents { agents: Vec<AgentId> },
    Department { department: String },
    Everyone { everyone: bool },
}

/// One operator board command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardCommand {
    /// Create a card.
    Create(CardCreateParams),
    /// List cards matching the optional filters.
    List(CardListParams),
    /// Claim a card for an agent.
    Claim {
        /// The card.
        card: CardId,
        /// The claimer.
        agent: AgentId,
        /// The sequence the operator observed.
        expected_seq: Seq,
    },
    /// Change a card's status.
    Move {
        /// The card.
        card: CardId,
        /// The new status.
        to: CardStatus,
        /// The sequence the operator observed.
        expected_seq: Seq,
    },
}

/// One `worktree` subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCommand {
    /// `worktree.list`.
    List,
    /// Record the removal of one released worktree, once the operator's own git has removed it.
    Remove {
        /// The lease.
        worktree: dudes_types::WorktreeId,
    },
}

/// One `workspace` subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceCommand {
    /// Append one entry, then have the daemon adopt exactly that addition.
    Add(Box<NewWorkspace>),
    /// Delete one entry, then have the daemon adopt exactly that removal.
    Remove {
        /// The workspace id.
        id: String,
    },
    /// Report the file's change against the running set, or apply it while its hash matches.
    Reload {
        /// The SHA-256 a previous check reported.
        apply: Option<String>,
    },
    /// The file's entries beside the running set.
    List,
    /// Add one row to a workspace's checks table, then have the daemon adopt that change.
    CheckAdd(Box<crate::workspace::NewCheck>),
    /// Delete one row of a workspace's checks table, then have the daemon adopt that change.
    CheckRemove {
        /// The workspace id.
        workspace: String,
        /// The check id.
        id: String,
    },
}

/// What was wrong with the arguments.
///
/// One variant per shape of mistake rather than a string, so the usage text is written once in
/// [`USAGE`] and the error says which flag it was about.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    /// No command word at all.
    #[error("no command given")]
    NoCommand,
    /// A command word this binary does not carry.
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    /// A flag this command does not take.
    #[error("unknown option `{0}`")]
    UnknownFlag(String),
    /// A flag that takes a value, given none.
    #[error("`{0}` needs a value")]
    MissingValue(&'static str),
    /// A required flag that was not given.
    #[error("`{0}` is required")]
    Missing(&'static str),
    /// A value that is not valid UTF-8, where the wire needs text.
    #[error("the value of `{0}` is not valid UTF-8, and the wire is JSON")]
    NotUtf8(&'static str),
    /// A value that did not parse as what the flag needs.
    #[error("`{flag}` does not take `{value}`: {what}")]
    BadValue {
        /// Which flag.
        flag: &'static str,
        /// What was given.
        value: String,
        /// What it should have been.
        what: &'static str,
    },
    /// A memory search input outside its query or hit bounds.
    #[error("memory search: {0}")]
    MemorySearchInput(#[from] dudes_proto::rpc::memory::MemorySearchInputError),
    /// `goal` was given no predicate argv after `--`.
    #[error("`goal` needs a predicate: `-- <program> [args...]`")]
    NoPredicate,
    /// A command word that takes a subcommand was given none.
    #[error("`{0}` needs a subcommand")]
    NoSubcommand(&'static str),
    /// A `config` key this binary does not manage.
    #[error("`{0}` is not a config key; the keys are {keys}", keys = config::key_list())]
    UnknownKey(String),
    /// `floor.chat` named in `config` or `--set`: Chat has no switch.
    #[error(
        "Chat is always on and has no setting; delete the `[floor.chat]` table from the \
         configuration file to stop the daemon warning about it"
    )]
    ChatAlwaysOn,
    /// `--json` on a command whose output has no contract type.
    #[error("`{0}` prints text only; it has no --json form")]
    TextOnly(&'static str),
    /// A flag that only means something beside another one, given without it.
    #[error("`{0}` needs {1}")]
    Requires(&'static str, &'static str),
    /// Two explicitly exclusive flags were given together.
    #[error("`{0}` cannot be combined with `{1}`")]
    Conflicting(&'static str, &'static str),
    /// **Not a mistake**: `--help` or `-h` in a flag position.
    ///
    /// It travels as an `Err` because it is the one outcome that produces no [`Invocation`], and
    /// [`crate::run`] answers it with [`USAGE`] on stdout and [`crate::exit::USAGE`] — the text the
    /// operator asked for, and the code that says nothing was sent. Carrying it *here*
    /// rather than in a pre-scan of argv is what keeps it correct: a pre-scan cannot see the `--`
    /// split and cannot see which tokens are flag **values**, so `goal … -- prog --help` and
    /// `--text help` both read as a help request, print the usage and exit `0` having sent
    /// nothing — a silent no-op reported as success on the door a loop drives.
    #[error("help requested")]
    HelpRequested,
}

/// The line after an [`ArgError`]'s message, pointing at [`USAGE`].
pub const HINT: &str = "Run `dudes --help` for every command and its options.";

/// The usage text, printed on `--help`.
pub const USAGE: &str = "\
dudes — the Dudes control CLI.

USAGE
    dudes [--json] <command> [options]

COMMANDS
    quickstart [--yes] [--restart] [--set <key> <value> ...]
               [--enroll [--check-label <text>] [--department <dept>]
                [--agent-usd-per-day <usd>] [--goal-usd <usd>]]
               [--hire <name> [--provider <provider>] [--role <role>] [--goal <text>]
                [--usd-per-day <n>] [--tokens-per-day <n>]]
               [--hop-cap <n>] [--timeout-s <n>] [-- <program> [args...]]
        Set up and start the daemon. On a terminal with no options it turns on the
        engines it finds on your PATH, then asks, one step at a time, whether the Floor
        may act on your agents, for a daily spending cap only when an API key is set,
        for the current repository's completion check (suggested from its files) and
        whether to hire a first agent. It then writes the operator file once, starts
        the daemon (or restarts one that runs other settings), shows where the daemon
        found each engine and whether it is signed in, prints the Floor's one-time
        link to paste into any browser, and makes the hire. Last, it offers to sign
        in to one engine that is signed out; the others are named with the dudes
        login command for each. It reads no engine's files.
        With options it asks nothing and prints no Floor link: it prints dudes floor
        open, and dudes login <engine> for each engine that is signed out. --yes
        takes every default, --set is `config set`, --enroll enrolls the current
        repository, --hire hires a first agent there, and everything after `--` is
        the completion check that the workspace and the goal share (default: at
        most 5 turns a goal, 600 seconds a check). A running daemon with other
        settings is restarted only with --restart, because a restart ends
        every running turn; without it quickstart writes the file, names what differs,
        stops nothing and exits 4. Before any restart the running daemon reads the new
        file, and a file it would refuse is put back as it was. Ceilings are
        optional.
        Without a terminal and without options it only starts the daemon when absent
        and prints its version, socket and journal position.

    start
        Start the daemon if it is not running, then print what ping prints. Use it
        after dudes stop, for example to run a newer version after an upgrade. It asks
        no questions, writes no configuration, prints no Floor link and hires nobody.
        If a daemon is already running it says so and leaves it alone, and when that
        daemon is not this version it says how to restart it.

    ping
        Ask the daemon for its version, socket and journal position.

    doctor
        Show daemon health and any stopped-journal recovery guidance.

    status
        Show current daemon status and any stopped-journal recovery guidance.

    stop [--hard]
        Announce closing time, wait for the bounded saving interval and teardown,
        then report the final acknowledgement census. Missing acknowledgements
        are named in a warning. An acknowledgement is not a verdict that work was saved.
        If the daemon crashed mid-closure, the restarted daemon completes that
        closure at once instead of granting a second saving interval.
        --hard ends running turns immediately and waits for teardown. After
        a managed turn it warns that it cannot prove nothing an agent process
        started left its process group or job; that warning alone is success.
        Any other teardown failure is an error.

    closing ack --announcement-lsn <lsn>
        Acknowledge your own participation using DUDES_TOKEN. The daemon derives
        the agent from that token and the connecting process.

    agents
        Every hired agent that is not retired: id, name, role, provider and status.

    team
        Colleagues: id, name, role, department, public status and first unfinished card.
        Uses DUDES_TOKEN when inherited, otherwise the operator credential.

    pause <agent>
        Hold future ordinary turns for this agent. Running turns and checks continue,
        and already-issued replay work is not canceled. Acknowledgement is not quiescence.

    resume <agent>
        Clear pause, halt and automatic hold for this agent. Review its controls first.

    halt --agent <id>
        Record a halt and request owned-child teardown. Use resume to clear the halt.
        Acknowledgement does not promise that teardown has finished.

    retire --agent <id> [--yes]
        Retire this agent. Resume cannot undo retirement. Without --yes, ask on an
        interactive terminal with default no. Scripts and --json require --yes.
        Acknowledgement does not promise that owned-child teardown has finished.

    goal replace --request <path>
        Submit one reviewed JSON request that binds text and a literal command check.
        The expected goal must still be current and all target work must have settled.
        Remaining optional budgets and hops are preserved. Retry the same file after
        a pending result or lost reply. No automatic pause, resume or goal rebasing occurs.
        The Floor shows only the safe check summary. Review the actual command locally.
        Exit 0 means changed, 3 means refused, and 4 means pending or a local error.

    pause --all
        Pause every current agent that is not already paused or halted. Each agent
        is admitted separately; a refusal is named and does not cancel later pauses.
        A running turn continues to its end. There is no resume-all command.

    hire --name <name> --role <role> --department <dept> --provider <provider>
         --cwd <path> [--usd-per-day <n>] [--tokens-per-day <n>] [--sandbox true|false]
         [--max-turns <n>] [--max-budget-usd <n>] [--trusted] [--mcp <server>]...
         [--worktree [--from <worktree-id>]]
         [--character c|d|e|f|g|h|i|j|k|l] [--accent amber|coral|mint|sky|violet|rose]
        Hire an agent. A relative --cwd is resolved against the current directory.
        With --worktree the daemon creates a git worktree of the repository
        --cwd is in, on a new branch, and the agent works there alone; it needs --trusted
        and --provider claude, and git 2.17 or later. --from starts the new tree at the
        tip of another agent's branch, so the new agent sees its commits and none of its
        uncommitted files. Dudes never merges, pushes or removes a tree itself: trees and
        branches stay until you run dudes worktree remove. Without --worktree, a hire into
        a directory another live agent already works in prints a warning, because their
        edits and checkouts then collide. The daemon refuses --sandbox at either value until an adapter
        can carry the switch; omit it for the provider's configured default.
        --max-turns and --max-budget-usd are optional positive backstops the provider
        enforces inside each run; omitting one emits no flag, and neither is a Dudes
        budget ceiling. Both are fixed for the agent's life. --trusted declares that you
        vouch for the repository, so the agent's runs may read its own settings and
        project files; it is recorded once at the hire, and omitting it means
        untrusted. It applies to Claude only: the daemon refuses --trusted for
        --provider codex and --provider pi. With isolation inherit, a trusted hire
        passes no --setting-sources, so the vendor's default set loads, including your
        user and local settings. --mcp opts the agent in to one MCP server declared under
        [mcp.servers.<server>] in the daemon's configuration; repeat it for more. The set
        can be replaced for later launches with dudes mcp. Codex and Pi take http servers only; a program
        server needs Claude and --trusted. Pi agents with servers need a tested release,
        currently 1.0.4; plain Pi agents need no version probe. A name or role holding a control character is refused. A role over 512 UTF-8 bytes is refused.

    worktree list
        Every worktree the daemon created that is not removed: id, state, agent, branch
        and path. A tree is pending until its agent is hired, active while the agent is
        live or has a recorded child, and released after that.

    worktree remove <worktree-id>
        Record that you removed one released worktree. While the tree exists this prints
        the git command to run, which is yours to run and review: git refuses a tree with
        modified or untracked files unless you add --force, and plain removal deletes
        ignored files too. Run this command again afterwards to record it. Refused while
        its agent is live or a recorded child remains. The branch is kept, so its commits
        stay. Dudes never removes a tree an agent ran in.

    rename <agent> <name>
        Change a live agent's display name. Its AgentId stays unchanged. A name holding
        a control character is refused.

    look <agent> --role <role> --character <c-l|automatic> --accent <colour|automatic>
        Change a live agent's display role and appearance. All fields are explicit;
        its engine and model stay fixed. A role holding a control character is refused.

    mcp <agent> [--mcp <server> ... | --clear]
        With no flags, read the desired MCP names. Repeat --mcp to replace the whole set,
        or use --clear to remove every name. A changed set is recorded for the next managed
        launch, preserving the agent's identity, memory and mail. Operator-only.

    goal --agent <id> --text <text> --hop-cap <n> [--usd <n>] [--tokens <n>]
         [--timeout-s <n>] [--check-id <id> --check-label <text>]
         [--check-may-already-pass] -- <program> [args...]
        Give an agent a goal. Everything after `--` is the predicate's argv: a typed
        list carried over the socket and resolved by the daemon, never a shell string.
        --check-id and --check-label name the check; the goal records the name and a
        revision of the check, and the Floor shows them in place of the command.
        The daemon runs the check once before the first turn and refuses it if it
        already passes, so Done shows that the agent's work made it pass. Add
        --check-may-already-pass for a check that may already pass, such as a test
        suite that must keep passing; it is then not run first.

    ask --text <text>
        Ask the human. Requires the per-agent token in DUDES_TOKEN; the daemon derives
        the asking agent from that token and the connecting process.

    why --agent <id>
        The last loop decision for an agent, with the gates it consulted.

    memory append --text <text> [--agent <id>]
    memory read [--since-lsn <position>] [--agent <id>]
    memory search <query> [--since-lsn <position>] [--limit <count>] [--agent <id>]
        Append to or read your own memory with DUDES_TOKEN. An operator supplies
        --agent instead. Read returns whole entries, at most 65536 text bytes,
        and the last included append's LSN. Use it with --since-lsn to read the
        next page. A position is segment:byte or a raw LSN from --json.
        An empty page has LSN 0. --json keeps the text exact.
        Search is literal and case-sensitive over original notes and the current
        summary. Query: 1 to 1024 UTF-8 bytes; limit: 1 to 100, default 10.
        Byte bounds can return fewer hits. A non-null next_since_lsn continues
        the live search; it does not freeze history. Search --agent requires the
        operator credential. A summary's read cursor starts its original range.

    mail send --to <agent[,agent...]|@department|@all> --act message|request
              --subject <text> [--body <text>]
    mail send --reply-to <mail> --act message|request --subject <text> [--body <text>]
    mail read [--unread]
    mail done <mail>
        Send, read or acknowledge mail in DUDES_MAILBOX without connecting to the
        daemon. Reply with a delivered mail id. These commands print text only.

    tail [--from <position>] [--follow]
        Journal entries from segment:byte or a raw LSN from --json (default: the tail).
        With --follow, keep reading.

    board create --title <title> --department <dept> [--depends-on <card> ...]
    board list [--department <dept>] [--status <status>] [--agent <id>]
    board claim --card <id> --agent <id> --expected-seq <n>
    board move --card <id> --to doing|review|blocked|done --expected-seq <n>
        Create, inspect, claim or move a journal-backed board card. Use the sequence
        from board list when claiming or moving; stale sequences are refused.

    workspace add <path> --id <id> --completion-check-label <text> --department <dept>
                  --hop-cap <n> --timeout-s <n> [--label <text>]
                  [--agent-usd-per-day <usd>] [--goal-usd <usd>]
                  [--tokens-per-day <n>] [--goal-tokens <n>]
                  -- <program> [args...]
        Enroll a repository directory for the Floor's Hire in the daemon's configuration,
        then have a running daemon adopt exactly that addition. <path> must be the
        absolute, canonical path of an existing directory holding .git.

    workspace remove <id>
        Remove one enrolled workspace from the configuration and the running daemon.
        No folder is touched.

    workspace reload [--apply <sha256>]
        Show how the configuration differs from what the daemon runs, with the file's
        SHA-256. With --apply, adopt that change only if the file still has that hash.

    workspace list
        The configuration's workspaces beside the ones the daemon runs.

    workspace check add <workspace> --id <id> --label <text> [--timeout-s <n>]
                        [--may-already-pass] -- <program> [args...]
    workspace check remove <workspace> <id>
        Add or remove one named completion check for an enrolled workspace, then have a
        running daemon adopt the change. The Floor offers a workspace's checks when it
        hires an agent or changes a goal there. A goal's check runs once before the first
        turn and must not already pass, unless --may-already-pass says Done may mean only
        that the check still passes after the agent's work. The check in the workspace's
        own entry changes through dudes workspace add.

    config
    config set <key> <value> [<key> <value> ...]
    config unset <key> [<key> ...]
        Show or edit the settings the daemon reads from its operator file when it
        starts. The engines: providers.codex and providers.pi, on or off.
        providers.codex.streaming opts in to streaming with a separate sign-in home;
        it defaults to off and requires macOS or Linux. After restarting, sign in with
        dudes login codex.
        The Floor's actions: floor.answer, floor.hire, floor.control, floor.goal,
        floor.renew, floor.retire, floor.terminal and floor.git, on or off.
        Chat is always on and has no setting. The budget: budget.floor_usd_per_day and
        budget.per_turn_floor.usd in dollars, budget.per_turn_floor.tokens in tokens.
        unset removes a key, so the daemon's default applies. One write carries every
        key given. An edit that breaks the file's own start rules is refused and names
        the key it needs. An edit that takes a per-turn reserve to zero is written with
        a warning, because the daemon refuses to start while an agent it has hired has
        a ceiling in that unit. With a daemon running, an edit names the settings it
        runs differently, which a restart applies: dudes stop, then dudes start.

    floor open [--no-browser]
        Mint a read-only Floor session and print its one-time sign-in link. Paste the
        link into any browser. It expires in 60 seconds and works once, in one
        browser; run the command again for a new one. Nothing is opened for you.
        --no-browser is accepted and changes nothing: printing is now the default.

    login claude|codex|pi
        Sign in to an engine with its own login command: `claude auth login` or
        `codex login`, run from the path the daemon found, in this terminal. It runs
        nothing when the daemon reports the engine signed in, and without a terminal
        or on Windows it prints the command instead. Codex streaming signs in to
        Dudes' separate home. Windows instructions use PowerShell. Pi signs in inside its own
        session: run pi, then type /login.

OPTIONS
    --json      Emit the contract's own types instead of a human summary. The summary
                writes each control or bidirectional formatting character in text
                from the daemon as \\u{...}; --json keeps that text exact.
    --help      This text.
    --version   Print the version.

ENVIRONMENT
    DUDES_MAILBOX       Your agent mailbox directory, required only by mail commands.
    DUDES_CONFIG        The operator file, as an absolute path; a relative one is refused.
                        Wins over the platform default on every system.
    DUDES_DAEMON        The daemon executable. Default: a `duded` sibling of this binary.
    DUDES_SOCK          The control socket. Default: <runtime dir>/duded.sock
    DUDES_RUNTIME_DIR   Where the daemon put its socket and operator credential.
                        Default: <storage root>/.local/state/dudes
    DUDES_STATE_DIR     Where the daemon keeps its journal and projections.
                        Default: <storage root>/.local/share/dudes
                        The storage root is absolute HOME, or on Windows with HOME
                        unset, absolute USERPROFILE. Directory overrides still win.
    DUDES_TOKEN         A per-agent token, when this process is an agent's child.

EXIT STATUS
    0  the requested local command completed or the daemon ran the method and answered
    1  nothing was sent: the arguments were wrong, or this text was asked for with
       other arguments
    2  no conversation happened: the socket was absent, unconnectable, or closed
       without answering
    3  the daemon understood the call and declined it
    4  anything else: another daemon error, or a local failure
";

/// Whether this token, **in a flag position**, asks for [`USAGE`].
///
/// "In a flag position" is the whole point, and it is not something this function checks — it is
/// established by every caller sitting inside a parser that has already consumed each value-taking
/// flag's value, and inside [`parse`]'s `--` split. That is why there is no `is_help(argv)` any
/// more: a scan of the whole argv sees a predicate's `--help` and a `--text help` alike, answers
/// both with the usage text, and exits `0` having sent nothing.
fn asks_for_help(arg: &OsStr) -> bool {
    arg == "--help" || arg == "-h"
}

/// Parse `argv` — the arguments **after** `argv[0]`.
///
/// # Errors
///
/// [`ArgError`] describing the first thing that was wrong.
pub fn parse(argv: &[OsString]) -> Result<Invocation, ArgError> {
    if let Some(at) = argv.iter().position(|arg| arg != "--json")
        && argv[at] == "mail"
    {
        let command = mail(&argv[at + 1..])?;
        if at != 0 {
            return Err(ArgError::TextOnly("mail"));
        }
        return Ok(Invocation {
            json: false,
            command: Command::Mail(command),
        });
    }
    parse_global(argv)
}

/// `command`, when nothing follows its word.
fn without_arguments(tail: &[OsString], command: Command) -> Result<Command, ArgError> {
    reject_leftovers(tail)?;
    Ok(command)
}

fn parse_global(argv: &[OsString]) -> Result<Invocation, ArgError> {
    // **Split at the first `--` before anything else looks at a flag.** Everything after it is a
    // command's own payload — today, a predicate's argv — and is never inspected for this CLI's
    // options. Without the split, `dudes goal ... -- cargo test --json` would lose an argument to a
    // flag of ours and the daemon would run a different command than the operator wrote.
    let (head, payload) = match argv.iter().position(|arg| arg == "--") {
        Some(at) => (&argv[..at], Some(&argv[at + 1..])),
        None => (argv, None),
    };

    // `--json` is accepted before or after the command word: an operator types it wherever it
    // occurs to them, and a flag whose position matters is a flag people get wrong.
    let mut json = false;
    let mut words: Vec<OsString> = Vec::with_capacity(head.len());
    for arg in head {
        if arg == "--json" {
            json = true;
        } else {
            words.push(arg.clone());
        }
    }
    let Some((command_word, tail)) = words.split_first() else {
        return Err(ArgError::NoCommand);
    };

    // The command word itself may be a help request — `dudes --help`, `dudes -h`, `dudes help`.
    // This is the **only** position where the bare word `help` means anything, which is what keeps
    // `hire --name help` a name and `--text help` a goal's text.
    if asks_for_help(command_word) || command_word == "help" {
        return Err(ArgError::HelpRequested);
    }

    let command = match command_word.to_string_lossy().as_ref() {
        "quickstart" => {
            let plan = quickstart(tail, payload)?;
            if json && plan.is_some() {
                return Err(ArgError::TextOnly("quickstart with setup options"));
            }
            Command::Quickstart(plan)
        }
        "start" => without_arguments(tail, Command::Start)?,
        "ping" => without_arguments(tail, Command::Ping)?,
        "doctor" | "status" => {
            reject_leftovers(tail)?;
            if command_word == "doctor" {
                Command::Doctor
            } else {
                Command::Status
            }
        }
        "stop" => stop(tail)?,
        "closing" => closing(tail)?,
        "agents" => {
            reject_leftovers(tail)?;
            Command::Agents
        }
        "team" => team(tail, payload)?,
        "pause" | "resume" => {
            targeted_control(command_word.to_string_lossy().as_ref(), tail, payload, json)?
        }
        "halt" | "retire" => {
            lifecycle_control(command_word.to_string_lossy().as_ref(), tail, payload)?
        }
        "hire" => Command::Hire(Box::new(hire(tail)?)),
        "rename" => {
            if payload.is_some() || tail.len() != 2 {
                return Err(ArgError::UnknownFlag(
                    "rename requires an agent and a name".to_owned(),
                ));
            }
            Command::Rename(RenameParams {
                agent: tail[0]
                    .to_str()
                    .ok_or(ArgError::NotUtf8("agent"))?
                    .parse()
                    .map_err(|_| ArgError::BadValue {
                        flag: "agent",
                        value: tail[0].to_string_lossy().into_owned(),
                        what: "an agent id",
                    })?,
                name: tail[1]
                    .to_str()
                    .ok_or(ArgError::NotUtf8("name"))?
                    .to_owned(),
            })
        }
        "look" => Command::Look(look(tail)?),
        "mcp" => Command::Mcp(mcp(tail, payload)?),
        "goal" => goal_command(tail, payload)?,
        "ask" => ask(tail)?,
        "memory" => Command::Memory(memory(tail)?),
        "why" => why(tail)?,
        "tail" => journal_tail(tail)?,
        "floor" => floor(tail)?,
        "login" if json => return Err(ArgError::TextOnly("login")),
        "login" => login(tail)?,
        "workspace" => Command::Workspace(workspace(tail, payload)?),
        "worktree" => Command::Worktree(worktree(tail)?),
        "board" => Command::Board(board(tail)?),
        "config" if json => return Err(ArgError::TextOnly("config")),
        "config" => Command::Config(config_command(tail)?),
        other => return Err(ArgError::UnknownCommand(other.to_owned())),
    };
    Ok(Invocation { json, command })
}

fn mail(tail: &[OsString]) -> Result<MailCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("mail"));
    };
    match subcommand.to_string_lossy().as_ref() {
        "--help" | "-h" => Err(ArgError::HelpRequested),
        "--json" => Err(ArgError::TextOnly("mail")),
        "send" => mail_send(rest).map(MailCommand::Send),
        "read" => {
            let mut unread = false;
            for flag in rest {
                match flag.to_string_lossy().as_ref() {
                    "--unread" if !unread => unread = true,
                    "--unread" => {
                        return Err(ArgError::Requires("--unread", "a single occurrence"));
                    }
                    "--help" | "-h" => return Err(ArgError::HelpRequested),
                    "--json" => return Err(ArgError::TextOnly("mail")),
                    other => return Err(ArgError::UnknownFlag(other.to_owned())),
                }
            }
            Ok(MailCommand::Read)
        }
        "done" => {
            for flag in rest {
                if asks_for_help(flag) {
                    return Err(ArgError::HelpRequested);
                }
                if flag == "--json" {
                    return Err(ArgError::TextOnly("mail"));
                }
            }
            match rest {
                [id] => canonical_mail_id(id).map(MailCommand::Done),
                [] => Err(ArgError::Missing("<mail>")),
                [_, extra, ..] => Err(ArgError::UnknownFlag(extra.to_string_lossy().into_owned())),
            }
        }
        other => Err(ArgError::UnknownCommand(format!("mail {other}"))),
    }
}

fn canonical_mail_id(value: &OsStr) -> Result<MailId, ArgError> {
    let text = value.to_str().ok_or(ArgError::NotUtf8("<mail>"))?;
    text.parse::<MailId>()
        .ok()
        .filter(|id| id.to_string() == text)
        .ok_or_else(|| ArgError::BadValue {
            flag: "<mail>",
            value: text.to_owned(),
            what: "a canonical mail id",
        })
}

fn mail_address(text: &str) -> Result<MailAddress, ArgError> {
    let bad = || ArgError::BadValue {
        flag: "--to",
        value: text.to_owned(),
        what: "one to seven canonical agent ids, @department or @all",
    };
    if text == "@all" {
        return Ok(MailAddress::Everyone { everyone: true });
    }
    if let Some(department) = text.strip_prefix('@') {
        if department
            .trim_matches(|c: char| c.is_ascii_whitespace())
            .is_empty()
        {
            return Err(bad());
        }
        return Ok(MailAddress::Department {
            department: department.to_owned(),
        });
    }
    let mut agents = std::collections::BTreeSet::new();
    for part in text.split(',') {
        let agent = part
            .parse::<AgentId>()
            .ok()
            .filter(|id| id.to_string() == part)
            .ok_or_else(bad)?;
        agents.insert(agent);
        if agents.len() > 7 {
            return Err(bad());
        }
    }
    let agents: Vec<_> = agents.into_iter().collect();
    Ok(if let [agent] = agents.as_slice() {
        MailAddress::Agent { agent: *agent }
    } else {
        MailAddress::Agents { agents }
    })
}

fn mail_send(tail: &[OsString]) -> Result<MailSend, ArgError> {
    let mut flags = Flags::new(tail);
    let (mut route, mut act, mut subject, mut body) = (None, None, None, None);
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = flags.next() {
        let flag = match arg.to_string_lossy().as_ref() {
            "--to" => "--to",
            "--reply-to" => "--reply-to",
            "--act" => "--act",
            "--subject" => "--subject",
            "--body" => "--body",
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            "--json" => return Err(ArgError::TextOnly("mail")),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        };
        if !seen.insert(flag) {
            return Err(ArgError::Requires(flag, "a single occurrence"));
        }
        let value = flags.value(flag)?;
        match flag {
            "--to" | "--reply-to" => {
                if route.is_some() {
                    return Err(ArgError::Requires("--to", "no --reply-to"));
                }
                route = Some(if flag == "--to" {
                    MailRoute::To(mail_address(&value)?)
                } else {
                    MailRoute::Reply(canonical_mail_id(OsStr::new(&value))?)
                });
            }
            "--act" => {
                act = Some(match value.as_str() {
                    "message" => dudes_proto::MailAct::Message,
                    "request" => dudes_proto::MailAct::Request,
                    _ => {
                        return Err(ArgError::BadValue {
                            flag,
                            value,
                            what: "message or request",
                        });
                    }
                });
            }
            "--subject" => subject = Some(value),
            "--body" => body = Some(value),
            _ => unreachable!(),
        }
    }
    Ok(MailSend {
        route: required(route, "--to or --reply-to")?,
        act: required(act, "--act")?,
        subject: required(subject, "--subject")?,
        body,
    })
}

fn unknown_key(word: String) -> ArgError {
    if word == "floor.chat" {
        ArgError::ChatAlwaysOn
    } else {
        ArgError::UnknownKey(word)
    }
}

#[cfg(test)]
mod mail_tests {
    use super::*;

    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn words(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn mail_values_preserve_flag_looking_tokens_and_exact_text() {
        for value in ["--json", "--help", "-h", "--", "$literal\n🦀"] {
            let invocation = parse(&words(&[
                "mail",
                "send",
                "--to",
                AGENT,
                "--act",
                "message",
                "--subject",
                value,
                "--body",
                value,
            ]))
            .unwrap();
            let Command::Mail(MailCommand::Send(send)) = invocation.command else {
                panic!("mail send")
            };
            assert_eq!(send.subject, value);
            assert_eq!(send.body.as_deref(), Some(value));
            assert!(!invocation.json);
        }
    }

    #[test]
    fn mail_rejects_machine_flags_help_operands_and_conflicting_routes() {
        for parts in [
            vec!["--json", "mail", "read"],
            vec!["mail", "read", "--json"],
            vec!["mail", "done", AGENT, "--json"],
            vec!["mail", "send", "--json"],
        ] {
            assert!(matches!(
                parse(&words(&parts)),
                Err(ArgError::TextOnly("mail"))
            ));
        }
        assert!(matches!(
            parse(&words(&["mail", "send", "--help"])),
            Err(ArgError::HelpRequested)
        ));
        for parts in [
            vec!["mail", "done", "../escape"],
            vec!["mail", "done", "01arz3ndektsv4rrffq69g5fav"],
            vec!["mail", "done", AGENT, "extra"],
            vec!["mail", "read", "--", "payload"],
            vec!["mail", "read", "--unread", "--unread"],
            vec!["mail", "send", "--to", AGENT, "--reply-to", AGENT],
            vec!["mail", "send", "--act", "card"],
            vec!["mail", "send", "--subject", "first", "--subject", "second"],
            vec![
                "mail",
                "send",
                "--act",
                "message",
                "--subject",
                "missing route",
            ],
        ] {
            assert!(parse(&words(&parts)).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn mail_addresses_deduplicate_and_replies_have_no_address() {
        let duplicate = format!("{AGENT},{AGENT}");
        assert_eq!(
            mail_address(&duplicate).unwrap(),
            MailAddress::Agent {
                agent: AGENT.parse().unwrap()
            }
        );
        let eight = (1..=8)
            .map(|n| AgentId::from_parts(1, n).to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(mail_address(&eight).is_err());
        assert!(mail_address("01arz3ndektsv4rrffq69g5fav").is_err());
        assert!(mail_address("@").is_err());
        assert_eq!(
            mail_address("@all").unwrap(),
            MailAddress::Everyone { everyone: true }
        );
        assert_eq!(
            mail_address("@ infra ").unwrap(),
            MailAddress::Department {
                department: " infra ".to_owned()
            }
        );
        let invocation = parse(&words(&[
            "mail",
            "send",
            "--reply-to",
            AGENT,
            "--act",
            "request",
            "--subject",
            "reply",
        ]))
        .unwrap();
        let Command::Mail(MailCommand::Send(send)) = invocation.command else {
            panic!("mail send")
        };
        assert_eq!(send.route, MailRoute::Reply(AGENT.parse().unwrap()));
        assert_eq!(send.act, dudes_proto::MailAct::Request);
        assert!(send.body.is_none());
    }
}

fn team(tail: &[OsString], payload: Option<&[OsString]>) -> Result<Command, ArgError> {
    reject_leftovers(tail)?;
    if payload.is_some() {
        return Err(ArgError::UnknownFlag("team takes no payload".to_owned()));
    }
    Ok(Command::Team)
}

fn stop(tail: &[OsString]) -> Result<Command, ArgError> {
    let mut flags = Flags::new(tail);
    let mut hard = false;
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--hard" => hard = true,
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(if hard {
        Command::StopHard
    } else {
        Command::Stop
    })
}

fn closing(tail: &[OsString]) -> Result<Command, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("closing"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    if subcommand != "ack" {
        return Err(ArgError::UnknownCommand(format!(
            "closing {}",
            subcommand.to_string_lossy()
        )));
    }
    let mut flags = Flags::new(rest);
    let mut announcement_lsn = None;
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--announcement-lsn" => {
                announcement_lsn = Some(Lsn(flags.parsed("--announcement-lsn", "an LSN")?));
            }
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(Command::ClosingAck {
        announcement_lsn: required(announcement_lsn, "--announcement-lsn")?,
    })
}

/// `config`, `config set <key> <value> ...` or `config unset <key> ...`.
fn config_command(tail: &[OsString]) -> Result<ConfigCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Ok(ConfigCommand::Show);
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    let words = rest
        .iter()
        .map(|word| word.to_str().ok_or(ArgError::NotUtf8("config")))
        .collect::<Result<Vec<_>, _>>()?;
    // Keys never start with `-`, so a help flag here is in a flag position.
    let key = |word: &str| match word {
        "--help" | "-h" => Err(ArgError::HelpRequested),
        _ => Key::named(word).ok_or_else(|| unknown_key(word.to_owned())),
    };
    let changes = match (subcommand.to_string_lossy().as_ref(), words.as_slice()) {
        ("set", []) => return Err(ArgError::Missing("<key> <value>")),
        ("unset", []) => return Err(ArgError::Missing("<key>")),
        ("set", _) => words
            .chunks(2)
            .map(|pair| {
                let key = key(pair[0])?;
                let text = pair.get(1).ok_or(ArgError::MissingValue(key.name()))?;
                Change::set(key, text).map_err(|what| ArgError::BadValue {
                    flag: key.name(),
                    value: (*text).to_owned(),
                    what,
                })
            })
            .collect::<Result<_, _>>()?,
        ("unset", _) => words
            .iter()
            .map(|word| key(word).map(Change::unset))
            .collect::<Result<_, _>>()?,
        (other, _) => return Err(ArgError::UnknownCommand(format!("config {other}"))),
    };
    Ok(ConfigCommand::Edit(changes))
}

fn board(tail: &[OsString]) -> Result<BoardCommand, ArgError> {
    let Some((command, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("board"));
    };
    if asks_for_help(command) {
        return Err(ArgError::HelpRequested);
    }
    let subcommand = command.to_string_lossy();
    if !matches!(subcommand.as_ref(), "create" | "list" | "claim" | "move") {
        return Err(ArgError::UnknownCommand(format!("board {subcommand}")));
    }
    let mut flags = Flags::new(rest);
    let (mut title, mut department, mut card, mut agent, mut status, mut seq) =
        (None, None, None, None, None, None);
    let mut deps = Vec::new();
    while let Some(arg) = flags.next() {
        match (subcommand.as_ref(), arg.to_string_lossy().as_ref()) {
            ("create", "--title") => title = Some(flags.value("--title")?),
            ("create" | "list", "--department") => department = Some(flags.value("--department")?),
            ("create", "--depends-on") => {
                deps.push(flags.parsed::<CardId>("--depends-on", "a card id")?);
            }
            ("claim" | "move", "--card") => {
                card = Some(flags.parsed::<CardId>("--card", "a card id")?);
            }
            ("claim" | "list", "--agent") => {
                agent = Some(flags.parsed::<AgentId>("--agent", "an agent id")?);
            }
            ("claim" | "move", "--expected-seq") => {
                seq = Some(Seq(flags.parsed::<u64>("--expected-seq", "a whole number")?));
            }
            ("list", "--status") => status = Some(board_status(&mut flags, "--status")?),
            ("move", "--to") => status = Some(board_status(&mut flags, "--to")?),
            (_, "--help" | "-h") => return Err(ArgError::HelpRequested),
            (_, other) => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    match subcommand.as_ref() {
        "create" => Ok(BoardCommand::Create(CardCreateParams {
            title: required(title, "--title")?,
            department: required(department, "--department")?,
            deps,
        })),
        "list" => Ok(BoardCommand::List(CardListParams {
            department,
            status,
            claimed_by: agent,
        })),
        "claim" => Ok(BoardCommand::Claim {
            card: required(card, "--card")?,
            agent: required(agent, "--agent")?,
            expected_seq: required(seq, "--expected-seq")?,
        }),
        "move" => Ok(BoardCommand::Move {
            card: required(card, "--card")?,
            to: required(status, "--to")?,
            expected_seq: required(seq, "--expected-seq")?,
        }),
        _ => Err(ArgError::UnknownCommand(format!("board {subcommand}"))),
    }
}

fn board_status(flags: &mut Flags<'_>, flag: &'static str) -> Result<CardStatus, ArgError> {
    let value = flags.value(flag)?;
    match value.as_str() {
        "todo" => Ok(CardStatus::Todo),
        "claimed" => Ok(CardStatus::Claimed),
        "doing" => Ok(CardStatus::Doing),
        "review" => Ok(CardStatus::Review),
        "blocked" => Ok(CardStatus::Blocked),
        "done" => Ok(CardStatus::Done),
        _ => Err(ArgError::BadValue {
            flag,
            value,
            what: "todo, claimed, doing, review, blocked or done",
        }),
    }
}

/// `floor open [--no-browser]`; the flag is accepted for scripts that still pass it.
///
/// The only subcommand is `open`. `dudes floor` alone is refused by name rather than folded into
/// `UnknownCommand`, because it is what an operator typing from memory will actually type.
fn floor(tail: &[OsString]) -> Result<Command, ArgError> {
    let Some((subcommand, flags)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("floor"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    if subcommand != "open" {
        return Err(ArgError::UnknownCommand(format!(
            "floor {}",
            subcommand.to_string_lossy()
        )));
    }
    for flag in flags {
        if asks_for_help(flag) {
            return Err(ArgError::HelpRequested);
        }
        if flag != "--no-browser" {
            return Err(ArgError::UnknownFlag(flag.to_string_lossy().into_owned()));
        }
    }
    Ok(Command::FloorOpen)
}

/// `login <claude|codex|pi>`.
fn login(tail: &[OsString]) -> Result<Command, ArgError> {
    match tail {
        [] => Err(ArgError::Missing("<engine>")),
        [word] if asks_for_help(word) => Err(ArgError::HelpRequested),
        [word] => {
            let text = word.to_str().ok_or(ArgError::NotUtf8("<engine>"))?;
            let provider = text
                .parse::<Provider>()
                .ok()
                .filter(|provider| {
                    matches!(provider, Provider::Claude | Provider::Codex | Provider::Pi)
                })
                .ok_or_else(|| ArgError::BadValue {
                    flag: "login",
                    value: text.to_owned(),
                    what: "claude, codex or pi",
                })?;
            Ok(Command::Login(provider))
        }
        [_, extra, ..] => Err(ArgError::UnknownFlag(extra.to_string_lossy().into_owned())),
    }
}

fn memory(tail: &[OsString]) -> Result<MemoryCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("memory"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    let subcommand = subcommand.to_string_lossy();
    if !matches!(subcommand.as_ref(), "append" | "read" | "search") {
        return Err(ArgError::UnknownCommand(format!("memory {subcommand}")));
    }
    let (query, rest) = if subcommand == "search" {
        let (query, rest) = rest.split_first().ok_or(ArgError::Missing("<query>"))?;
        if asks_for_help(query) {
            return Err(ArgError::HelpRequested);
        }
        (
            Some(
                query
                    .clone()
                    .into_string()
                    .map_err(|_| ArgError::NotUtf8("<query>"))?,
            ),
            rest,
        )
    } else {
        (None, rest)
    };
    let mut flags = Flags::new(rest);
    let (mut agent, mut text, mut since_lsn, mut limit) = (None, None, None, None);
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--agent" => agent = Some(flags.parsed("--agent", "an agent id")?),
            "--text" if subcommand == "append" => text = Some(flags.value("--text")?),
            "--since-lsn" if subcommand == "read" || subcommand == "search" => {
                since_lsn = Some(flags.position("--since-lsn")?);
            }
            "--limit" if subcommand == "search" => {
                limit = Some(flags.parsed("--limit", "a count from 1 through 100")?);
            }
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(if subcommand == "append" {
        MemoryCommand::Append {
            agent,
            text: required(text, "--text")?,
        }
    } else if subcommand == "read" {
        MemoryCommand::Read { agent, since_lsn }
    } else {
        MemoryCommand::Search {
            agent,
            options: dudes_proto::rpc::memory::MemorySearchOptions::new(
                query.expect("search query"),
                since_lsn,
                limit,
            )?,
        }
    })
}

/// `workspace add|remove|reload|list`.
fn workspace(
    tail: &[OsString],
    payload: Option<&[OsString]>,
) -> Result<WorkspaceCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("workspace"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    match subcommand.to_string_lossy().as_ref() {
        "add" => workspace_add(rest, payload).map(|new| WorkspaceCommand::Add(Box::new(new))),
        "remove" => match rest {
            [] => Err(ArgError::Missing("<id>")),
            [first, ..] if asks_for_help(first) => Err(ArgError::HelpRequested),
            [id] => Ok(WorkspaceCommand::Remove {
                id: id
                    .clone()
                    .into_string()
                    .map_err(|_| ArgError::NotUtf8("<id>"))?,
            }),
            [_, extra, ..] => Err(ArgError::UnknownFlag(extra.to_string_lossy().into_owned())),
        },
        "reload" => {
            let mut flags = Flags::new(rest);
            let mut apply = None;
            while let Some(arg) = flags.next() {
                match arg.to_string_lossy().as_ref() {
                    "--apply" => apply = Some(flags.value("--apply")?),
                    "--help" | "-h" => return Err(ArgError::HelpRequested),
                    other => return Err(ArgError::UnknownFlag(other.to_owned())),
                }
            }
            Ok(WorkspaceCommand::Reload { apply })
        }
        "list" => {
            reject_leftovers(rest)?;
            Ok(WorkspaceCommand::List)
        }
        "check" => workspace_check(rest, payload),
        other => Err(ArgError::UnknownCommand(format!("workspace {other}"))),
    }
}

/// `workspace check add <workspace> --id <id> --label <label> [--timeout-s <n>]
/// [--may-already-pass] -- <program> [args...]` and `workspace check remove <workspace> <id>`.
fn workspace_check(
    tail: &[OsString],
    payload: Option<&[OsString]>,
) -> Result<WorkspaceCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("workspace check"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    let word = |part: &OsString, what: &'static str| {
        part.clone()
            .into_string()
            .map_err(|_| ArgError::NotUtf8(what))
    };
    match subcommand.to_string_lossy().as_ref() {
        "remove" => match rest {
            [first, ..] if asks_for_help(first) => Err(ArgError::HelpRequested),
            [workspace, id] => Ok(WorkspaceCommand::CheckRemove {
                workspace: word(workspace, "<workspace>")?,
                id: word(id, "<id>")?,
            }),
            [] => Err(ArgError::Missing("<workspace>")),
            [_] => Err(ArgError::Missing("<id>")),
            [_, _, extra, ..] => Err(ArgError::UnknownFlag(extra.to_string_lossy().into_owned())),
        },
        "add" => {
            let mut flags = Flags::new(rest);
            let (mut workspace, mut id, mut label) = (None, None, None);
            let (mut timeout_s, mut may_already_pass) = (DEFAULT_PREDICATE_TIMEOUT_S, false);
            while let Some(arg) = flags.next() {
                match arg.to_string_lossy().as_ref() {
                    "--id" => id = Some(flags.value("--id")?),
                    "--label" => label = Some(flags.value("--label")?),
                    "--timeout-s" => timeout_s = flags.parsed::<u64>("--timeout-s", "seconds")?,
                    "--may-already-pass" => may_already_pass = true,
                    "--help" | "-h" => return Err(ArgError::HelpRequested),
                    other if other.starts_with('-') => {
                        return Err(ArgError::UnknownFlag(other.to_owned()));
                    }
                    _ if workspace.is_none() => workspace = Some(word(arg, "<workspace>")?),
                    other => return Err(ArgError::UnknownFlag(other.to_owned())),
                }
            }
            if timeout_s == 0 {
                return Err(ArgError::BadValue {
                    flag: "--timeout-s",
                    value: "0".to_owned(),
                    what: "seconds above zero",
                });
            }
            let argv = payload.unwrap_or_default();
            if argv.is_empty() {
                return Err(ArgError::NoPredicate);
            }
            Ok(WorkspaceCommand::CheckAdd(Box::new(
                crate::workspace::NewCheck {
                    workspace: required(workspace, "<workspace>")?,
                    id: required(id, "--id")?,
                    label: required(label, "--label")?,
                    argv: argv
                        .iter()
                        .map(|part| word(part, "--"))
                        .collect::<Result<_, _>>()?,
                    timeout_s,
                    may_already_pass,
                },
            )))
        }
        other => Err(ArgError::UnknownCommand(format!("workspace check {other}"))),
    }
}

fn workspace_add(
    tail: &[OsString],
    payload: Option<&[OsString]>,
) -> Result<NewWorkspace, ArgError> {
    let mut flags = Flags::new(tail);
    let (mut path, mut id, mut label, mut check, mut department) = (None, None, None, None, None);
    let (mut agent_usd, mut goal_usd, mut hop_cap, mut timeout_s) = (None, None, None, None);
    let (mut tokens_per_day, mut goal_tokens) = (None, None);
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--id" => id = Some(flags.value("--id")?),
            "--label" => label = Some(flags.value("--label")?),
            "--completion-check-label" => check = Some(flags.value("--completion-check-label")?),
            "--department" => department = Some(flags.value("--department")?),
            "--agent-usd-per-day" => agent_usd = Some(usd(&mut flags, "--agent-usd-per-day")?),
            "--goal-usd" => goal_usd = Some(usd(&mut flags, "--goal-usd")?),
            "--hop-cap" => hop_cap = Some(flags.parsed::<u32>("--hop-cap", "a whole number")?),
            "--timeout-s" => timeout_s = Some(flags.parsed::<u64>("--timeout-s", "seconds")?),
            "--tokens-per-day" => {
                tokens_per_day = Some(flags.parsed::<u64>("--tokens-per-day", "a whole number")?);
            }
            "--goal-tokens" => {
                goal_tokens = Some(flags.parsed::<u64>("--goal-tokens", "a whole number")?);
            }
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other if other.starts_with('-') => return Err(ArgError::UnknownFlag(other.to_owned())),
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    let argv = payload.unwrap_or_default();
    if argv.is_empty() {
        return Err(ArgError::NoPredicate);
    }
    let predicate_argv = argv
        .iter()
        .map(|part| {
            part.clone()
                .into_string()
                .map_err(|_| ArgError::NotUtf8("--"))
        })
        .collect::<Result<_, _>>()?;
    let id = required(id, "--id")?;
    Ok(NewWorkspace {
        path: required(path, "<path>")?,
        label: label.unwrap_or_else(|| id.clone()),
        id,
        completion_check_label: required(check, "--completion-check-label")?,
        department: required(department, "--department")?,
        agent_usd_per_day: agent_usd,
        goal_usd,
        hop_cap: required(hop_cap, "--hop-cap")?,
        predicate_argv,
        predicate_timeout_s: required(timeout_s, "--timeout-s")?,
        tokens_per_day,
        goal_tokens,
    })
}

/// A positive dollar amount with at most six decimal places, kept as the text typed so the file
/// holds exactly what the operator wrote.
fn usd(flags: &mut Flags<'_>, flag: &'static str) -> Result<String, ArgError> {
    let value = flags.value(flag)?;
    if positive_dollars(&value) {
        Ok(value)
    } else {
        Err(ArgError::BadValue {
            flag,
            value,
            what: "a positive dollar amount with at most six decimal places",
        })
    }
}

/// Whether `text` is a positive dollar amount with at most six decimal places.
pub(crate) fn positive_dollars(text: &str) -> bool {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
    !whole.is_empty()
        && digits(whole)
        && (whole == "0" || !whole.starts_with('0'))
        && digits(fraction)
        && fraction.len() <= 6
        && text.contains('.') != fraction.is_empty()
        && text.bytes().any(|byte| (b'1'..=b'9').contains(&byte))
}

/// The completion check `quickstart` asks for when it has none.
const CHECK: &str = "a completion check after `--`: `-- <program> [args...]`";

/// `quickstart`'s options as a [`Plan`]; `None` when it was given none.
fn quickstart(
    tail: &[OsString],
    payload: Option<&[OsString]>,
) -> Result<Option<Box<Plan>>, ArgError> {
    if tail.is_empty() && payload.is_none() {
        return Ok(None);
    }
    let mut flags = Flags::new(tail);
    let mut given = QuickstartFlags::default();
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--yes" => {}
            "--restart" => given.restart = true,
            "--set" => {
                let word = flags.value("--set")?;
                let key = Key::named(&word).ok_or_else(|| unknown_key(word))?;
                let text = flags.value(key.name())?;
                let change = Change::set(key, &text).map_err(|what| ArgError::BadValue {
                    flag: key.name(),
                    value: text.clone(),
                    what,
                })?;
                given.changes.push(change);
            }
            "--enroll" => given.enroll = true,
            "--check-label" => given.check_label = Some(flags.value("--check-label")?),
            "--department" => {
                let value = flags.value("--department")?;
                let department = setup::display_text(&value).map_err(|_| ArgError::BadValue {
                    flag: "--department",
                    value,
                    what: "1 to 128 characters with no control character",
                })?;
                given.department = Some(department);
            }
            "--agent-usd-per-day" => {
                given.agent_usd_per_day = Some(usd(&mut flags, "--agent-usd-per-day")?);
            }
            "--goal-usd" => given.goal_usd = Some(usd(&mut flags, "--goal-usd")?),
            "--hire" => given.name = Some(flags.value("--hire")?),
            "--provider" => {
                given.provider = Some(flags.parsed::<Provider>("--provider", "a known provider")?);
            }
            "--role" => given.role = Some(flags.value("--role")?),
            "--goal" => given.goal = Some(flags.value("--goal")?),
            "--usd-per-day" => {
                given.usd_per_day = Some(flags.parsed::<u64>("--usd-per-day", "whole dollars")?);
            }
            "--tokens-per-day" => {
                given.tokens_per_day =
                    Some(flags.parsed::<u64>("--tokens-per-day", "a whole number")?);
            }
            "--hop-cap" => {
                given.hop_cap = Some(above_zero(&mut flags, "--hop-cap")?);
            }
            "--timeout-s" => {
                given.timeout_s = Some(above_zero(&mut flags, "--timeout-s")?);
            }
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    let argv = payload
        .unwrap_or_default()
        .iter()
        .map(|part| {
            part.clone()
                .into_string()
                .map_err(|_| ArgError::NotUtf8("--"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    given.into_plan(argv).map(|plan| Some(Box::new(plan)))
}

/// A whole number above zero, which the daemon needs for a hop cap and a check's timeout.
fn above_zero<T: std::str::FromStr + Default + PartialEq>(
    flags: &mut Flags<'_>,
    flag: &'static str,
) -> Result<T, ArgError> {
    let value = flags.value(flag)?;
    value
        .parse::<T>()
        .ok()
        .filter(|number| *number != T::default())
        .ok_or(ArgError::BadValue {
            flag,
            value,
            what: "a whole number above zero",
        })
}

/// Every option `quickstart` was given, before they are checked against each other.
#[derive(Default)]
struct QuickstartFlags {
    restart: bool,
    changes: Vec<Change>,
    enroll: bool,
    check_label: Option<String>,
    department: Option<String>,
    agent_usd_per_day: Option<String>,
    goal_usd: Option<String>,
    name: Option<String>,
    provider: Option<Provider>,
    role: Option<String>,
    goal: Option<String>,
    usd_per_day: Option<u64>,
    tokens_per_day: Option<u64>,
    hop_cap: Option<u32>,
    timeout_s: Option<u64>,
}

impl QuickstartFlags {
    /// The plan these options and the check's `argv` make, or the first option that needs another.
    fn into_plan(self, argv: Vec<String>) -> Result<Plan, ArgError> {
        let Self {
            restart,
            changes,
            enroll,
            check_label,
            department,
            agent_usd_per_day,
            goal_usd,
            name,
            provider,
            role,
            goal,
            usd_per_day,
            tokens_per_day,
            hop_cap,
            timeout_s,
        } = self;
        let enroll_only = [
            ("--check-label", check_label.is_some()),
            ("--department", department.is_some()),
            ("--agent-usd-per-day", agent_usd_per_day.is_some()),
            ("--goal-usd", goal_usd.is_some()),
        ];
        if let Some(&(flag, _)) = enroll_only.iter().find(|(_, given)| *given && !enroll) {
            return Err(ArgError::Requires(flag, "--enroll"));
        }
        let hire_only = [
            ("--provider", provider.is_some()),
            ("--role", role.is_some()),
            ("--goal", goal.is_some()),
            ("--usd-per-day", usd_per_day.is_some()),
            ("--tokens-per-day", tokens_per_day.is_some()),
        ];
        if let Some(&(flag, _)) = hire_only.iter().find(|(_, given)| *given && name.is_none()) {
            return Err(ArgError::Requires(flag, "--hire <name>"));
        }
        match (argv.is_empty(), enroll, goal.is_some()) {
            (true, true, _) => return Err(ArgError::Requires("--enroll", CHECK)),
            (true, false, true) => return Err(ArgError::Requires("--goal", CHECK)),
            (false, false, false) => return Err(ArgError::Requires("--", "--enroll or --goal")),
            (true, false, false) if hop_cap.is_some() || timeout_s.is_some() => {
                return Err(ArgError::Requires("--hop-cap and --timeout-s", CHECK));
            }
            _ => {}
        }
        let check = (!argv.is_empty()).then(|| Check {
            argv,
            timeout_s: timeout_s.unwrap_or(DEFAULT_PREDICATE_TIMEOUT_S),
            hop_cap: hop_cap.unwrap_or(setup::DEFAULT_HOP_CAP),
        });
        let enroll = match (enroll, &check) {
            (true, Some(check)) => {
                let label = check_label.unwrap_or_else(|| check.argv.join(" "));
                let check_label = setup::display_text(&label).map_err(|_| ArgError::BadValue {
                    flag: "--check-label",
                    value: label,
                    what: "1 to 128 characters with no control character",
                })?;
                Some(Enroll {
                    check_label,
                    department: department.unwrap_or_else(|| setup::DEFAULT_DEPARTMENT.to_owned()),
                    agent_usd_per_day,
                    goal_usd,
                })
            }
            _ => None,
        };
        let hire = name.map(|name| Hire {
            name,
            role: role.unwrap_or_else(|| setup::DEFAULT_ROLE.to_owned()),
            provider: provider.unwrap_or(Provider::Claude),
            goal,
            usd_per_day,
            tokens_per_day,
        });
        Ok(Plan {
            changes,
            check,
            enroll,
            hire,
            restart,
        })
    }
}

/// A command that takes no options was given one.
fn reject_leftovers(tail: &[OsString]) -> Result<(), ArgError> {
    match tail.first() {
        None => Ok(()),
        // `ping --help` is a help request, not an unknown flag. `ping` takes no value-bearing flag
        // at all, so every token here is in a flag position by construction.
        Some(arg) if asks_for_help(arg) => Err(ArgError::HelpRequested),
        Some(arg) => Err(ArgError::UnknownFlag(arg.to_string_lossy().into_owned())),
    }
}

/// A cursor over one command's arguments, pairing each flag with its value.
struct Flags<'a> {
    args: std::slice::Iter<'a, OsString>,
}

impl<'a> Flags<'a> {
    fn new(tail: &'a [OsString]) -> Self {
        Self { args: tail.iter() }
    }

    /// The next argument, whatever it is.
    fn next(&mut self) -> Option<&'a OsString> {
        self.args.next()
    }

    /// The value of `flag`, as text.
    fn value(&mut self, flag: &'static str) -> Result<String, ArgError> {
        self.args
            .next()
            .ok_or(ArgError::MissingValue(flag))?
            .clone()
            .into_string()
            .map_err(|_| ArgError::NotUtf8(flag))
    }

    /// The value of `flag`, as a path — no UTF-8 requirement, because a path is not text.
    fn path(&mut self, flag: &'static str) -> Result<PathBuf, ArgError> {
        Ok(PathBuf::from(
            self.args.next().ok_or(ArgError::MissingValue(flag))?,
        ))
    }

    /// The value of `flag`, parsed.
    fn parsed<T: std::str::FromStr>(
        &mut self,
        flag: &'static str,
        what: &'static str,
    ) -> Result<T, ArgError> {
        let value = self.value(flag)?;
        value
            .parse()
            .map_err(|_| ArgError::BadValue { flag, value, what })
    }

    fn position(&mut self, flag: &'static str) -> Result<Lsn, ArgError> {
        let value = self.value(flag)?;
        let position = match value.split_once(':') {
            Some((segment, byte)) => segment
                .parse()
                .ok()
                .zip(byte.parse().ok())
                .and_then(|(segment, byte)| Lsn::from_parts(segment, byte)),
            None => value.parse().ok().map(Lsn),
        };
        position.ok_or(ArgError::BadValue {
            flag,
            value,
            what: "a journal position as segment:byte (for example, 1:2048) or a raw LSN",
        })
    }
}

/// A required flag that was given, or [`ArgError::Missing`].
fn required<T>(value: Option<T>, flag: &'static str) -> Result<T, ArgError> {
    value.ok_or(ArgError::Missing(flag))
}

fn mcp(tail: &[OsString], payload: Option<&[OsString]>) -> Result<McpParams, ArgError> {
    if payload.is_some() {
        return Err(ArgError::UnknownFlag(
            "mcp takes no payload after --".into(),
        ));
    }
    let Some(first) = tail.first() else {
        return Err(ArgError::Missing("agent"));
    };
    if asks_for_help(first) {
        return Err(ArgError::HelpRequested);
    }
    let value = first.to_str().ok_or(ArgError::NotUtf8("agent"))?;
    let agent = value.parse().map_err(|_| ArgError::BadValue {
        flag: "agent",
        value: value.to_owned(),
        what: "an agent id",
    })?;
    let mut flags = Flags::new(&tail[1..]);
    let mut names = Vec::<dudes_types::McpServerName>::new();
    let mut clear = false;
    while let Some(flag) = flags.next() {
        match flag.to_string_lossy().as_ref() {
            "--mcp" => {
                let name: dudes_types::McpServerName =
                    flags.parsed("--mcp", "a declared MCP server name")?;
                if names.contains(&name) {
                    return Err(ArgError::BadValue {
                        flag: "--mcp",
                        value: name.as_str().to_owned(),
                        what: "each server name at most once",
                    });
                }
                names.push(name);
            }
            "--clear" => clear = true,
            _ if asks_for_help(flag) => return Err(ArgError::HelpRequested),
            _ => return Err(ArgError::UnknownFlag(flag.to_string_lossy().into_owned())),
        }
    }
    if clear && !names.is_empty() {
        return Err(ArgError::Conflicting("--clear", "--mcp"));
    }
    let replacement = clear || !names.is_empty();
    names.sort();
    Ok(McpParams {
        agent,
        mcp_servers: replacement.then_some(names),
    })
}

fn look(tail: &[OsString]) -> Result<LookParams, ArgError> {
    let Some(first) = tail.first() else {
        return Err(ArgError::Missing("agent"));
    };
    let agent_text = first.to_str().ok_or(ArgError::NotUtf8("agent"))?;
    let agent = agent_text.parse().map_err(|_| ArgError::BadValue {
        flag: "agent",
        value: agent_text.to_owned(),
        what: "an agent id",
    })?;
    let mut flags = Flags::new(&tail[1..]);
    let (mut role, mut character, mut accent) = (None, None, None);
    while let Some(flag) = flags.next() {
        match flag.to_string_lossy().as_ref() {
            "--role" => role = Some(flags.value("--role")?),
            "--character" => {
                let value = flags.value("--character")?;
                character = Some(if value == "automatic" {
                    None
                } else {
                    Some(value.parse().map_err(|_| ArgError::BadValue {
                        flag: "--character",
                        value,
                        what: "c through l, or automatic",
                    })?)
                });
            }
            "--accent" => {
                let value = flags.value("--accent")?;
                accent = Some(if value == "automatic" {
                    None
                } else {
                    Some(value.parse().map_err(|_| ArgError::BadValue {
                        flag: "--accent",
                        value,
                        what: "amber, coral, mint, sky, violet, rose, or automatic",
                    })?)
                });
            }
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(LookParams {
        agent,
        role: required(role, "--role")?,
        character: required(character, "--character")?,
        accent: required(accent, "--accent")?,
    })
}

fn worktree(tail: &[OsString]) -> Result<WorktreeCommand, ArgError> {
    let Some((subcommand, rest)) = tail.split_first() else {
        return Err(ArgError::NoSubcommand("worktree"));
    };
    if asks_for_help(subcommand) {
        return Err(ArgError::HelpRequested);
    }
    match subcommand.to_string_lossy().as_ref() {
        "list" => {
            reject_leftovers(rest)?;
            Ok(WorktreeCommand::List)
        }
        "remove" => {
            let mut id = None;
            for arg in rest {
                match arg.to_string_lossy().as_ref() {
                    "--help" | "-h" => return Err(ArgError::HelpRequested),
                    flag if flag.starts_with('-') => {
                        return Err(ArgError::UnknownFlag(flag.to_owned()));
                    }
                    _ if id.is_none() => {
                        id = Some(
                            arg.to_str()
                                .ok_or(ArgError::NotUtf8("<id>"))?
                                .parse::<dudes_types::WorktreeId>()
                                .map_err(|_| ArgError::BadValue {
                                    flag: "<id>",
                                    value: arg.to_string_lossy().into_owned(),
                                    what: "a worktree id (32 lowercase hexadecimal characters)",
                                })?,
                        );
                    }
                    other => return Err(ArgError::UnknownFlag(other.to_owned())),
                }
            }
            Ok(WorktreeCommand::Remove {
                worktree: id.ok_or(ArgError::Missing("<id>"))?,
            })
        }
        other => Err(ArgError::UnknownCommand(format!("worktree {other}"))),
    }
}

fn hire(tail: &[OsString]) -> Result<HireParams, ArgError> {
    let mut flags = Flags::new(tail);
    let (mut name, mut role, mut department, mut provider, mut cwd) =
        (None, None, None, None, None);
    let (mut usd_per_day, mut tokens_per_day, mut sandbox) = (None, None, None);
    let (mut max_turns, mut max_budget_usd) = (None, None);
    let (mut character, mut accent) = (None, None);
    let mut trusted = false;
    let (mut worktree, mut from) = (false, None);
    let mut mcp_servers = Vec::new();

    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--name" => name = Some(flags.value("--name")?),
            "--trusted" => trusted = true,
            "--mcp" => mcp_servers.push(flags.parsed::<dudes_types::McpServerName>(
                "--mcp",
                "a server name from [mcp.servers] in the daemon's configuration",
            )?),
            "--worktree" => worktree = true,
            "--from" => {
                from = Some(flags.parsed::<dudes_types::WorktreeId>(
                    "--from",
                    "a worktree id from `dudes worktree list`",
                )?);
            }
            "--character" => {
                character = Some(flags.parsed::<dudes_types::CastCharacter>(
                    "--character",
                    "an authored cast character (c through l)",
                )?);
            }
            "--accent" => {
                accent = Some(flags.parsed::<dudes_types::AccentColour>(
                    "--accent",
                    "amber, coral, mint, sky, violet or rose",
                )?);
            }
            "--role" => role = Some(flags.value("--role")?),
            "--department" => department = Some(flags.value("--department")?),
            "--provider" => {
                provider = Some(flags.parsed::<Provider>("--provider", "a known provider")?);
            }
            "--cwd" => cwd = Some(flags.path("--cwd")?),
            "--usd-per-day" => {
                usd_per_day = Some(flags.parsed::<u64>("--usd-per-day", "whole dollars")?);
            }
            "--tokens-per-day" => {
                tokens_per_day = Some(flags.parsed::<u64>("--tokens-per-day", "a whole number")?);
            }
            "--sandbox" => sandbox = Some(flags.parsed::<bool>("--sandbox", "true or false")?),
            "--max-turns" => {
                max_turns = Some(flags.parsed::<u32>("--max-turns", "a positive whole number")?);
            }
            "--max-budget-usd" => {
                max_budget_usd =
                    Some(flags.parsed::<u64>("--max-budget-usd", "positive whole dollars")?);
            }
            // Reached only in a flag position: every value-taking flag above has already
            // consumed its value, so a `--help` here is this CLI's and never an operand.
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }

    if from.is_some() && !worktree {
        return Err(ArgError::Requires("--from", "--worktree"));
    }
    if worktree && !trusted {
        return Err(ArgError::Requires("--worktree", "--trusted"));
    }
    Ok(HireParams {
        character,
        accent,
        name: required(name, "--name")?,
        role: required(role, "--role")?,
        department: required(department, "--department")?,
        provider: required(provider, "--provider")?,
        cwd: required(cwd, "--cwd")?,
        // Both units are optional and both mean the same three things:
        // omitted is no ceiling, `0` is a ceiling of nothing, `n` is a ceiling of `n`. An omitted
        // flag is `None` rather than a default, for the reason `--sandbox` below is: a CLI that
        // invented a number would put a ceiling on the wire that the operator never typed.
        budget: AgentBudget {
            usd_per_day: usd_per_day.map(Usd::from_dollars),
            tokens_per_day: tokens_per_day.map(Tokens::new),
        },
        // `None` leaves the provider's configured default alone; an explicit value of either
        // polarity is refused by the daemon. The CLI does not second-guess the contract
        // in either direction: it neither defaults to `Some(true)` nor swallows the flag, so the
        // operator reads the refusal from the daemon rather than from here.
        sandbox,
        // The CLI has no per-agent isolation flag yet; omission leaves the provider's configured
        // selection in force while direct RPC clients may use the additive contract field.
        isolation: None,
        // Omission is `None` — no flag composed and no Dudes default — and a typed `0` is
        // rejected here rather than sent, because zero has no meaning at all and
        // this CLI has the value in hand.
        max_turns: positive("--max-turns", max_turns)?,
        max_budget: max_budget_usd.map(dollars_to_usd).transpose()?,
        trusted,
        checkout: worktree.then_some(dudes_proto::rpc::agent::Checkout::Worktree),
        from,
        mcp_servers,
    })
}

/// `Some(n)` for a positive `n`, or [`ArgError::BadValue`] for a typed `0`.
fn positive(flag: &'static str, value: Option<u32>) -> Result<Option<u32>, ArgError> {
    match value {
        Some(0) => Err(ArgError::BadValue {
            flag,
            value: "0".to_owned(),
            what: "a positive whole number; omit the flag for no cap",
        }),
        other => Ok(other),
    }
}

/// Whole dollars as the operator typed them, converted to the micro-dollars the wire carries.
///
/// **Checked, and that is the point of the function.** [`Usd::from_dollars`] saturates, so a
/// `--max-budget-usd` above `u64::MAX / 1_000_000` would silently become a backstop of roughly
/// eighteen trillion dollars instead of the refusal an operator who typed a wrong number needs.
/// This multiplies with [`u64::checked_mul`] and refuses the overflow before any `Invocation` or
/// `HireParams` is built, so nothing unsendable reaches the socket and nothing wrong reaches the
/// journal. Zero is refused for the reason: it is neither no cap nor a cap.
fn dollars_to_usd(dollars: u64) -> Result<Usd, ArgError> {
    const FLAG: &str = "--max-budget-usd";
    if dollars == 0 {
        return Err(ArgError::BadValue {
            flag: FLAG,
            value: "0".to_owned(),
            what: "positive whole dollars; omit the flag for no backstop",
        });
    }
    dollars
        .checked_mul(1_000_000)
        .map(Usd::from_micros)
        .ok_or_else(|| ArgError::BadValue {
            flag: FLAG,
            value: dollars.to_string(),
            what: "whole dollars that fit in micro-dollars (at most 18446744073709)",
        })
}

/// The default a predicate command is killed after, in seconds.
///
/// Not this binary's number: it is the value `bins/duded`'s own fixtures use, and the one the
/// contract's `timeout_s` is documented against. `--timeout-s` overrides it.
pub(crate) const DEFAULT_PREDICATE_TIMEOUT_S: u64 = 600;

fn targeted_control(
    name: &str,
    tail: &[OsString],
    payload: Option<&[OsString]>,
    json: bool,
) -> Result<Command, ArgError> {
    if name == "pause" && tail == [OsStr::new("--all")] && payload.is_none() {
        if json {
            return Err(ArgError::TextOnly("pause --all"));
        }
        return Ok(Command::PauseAll);
    }
    if tail.len() != 1 || payload.is_some() {
        return Err(ArgError::UnknownFlag(format!(
            "{name} requires one agent id"
        )));
    }
    let agent = tail[0]
        .to_str()
        .ok_or(ArgError::NotUtf8("agent"))?
        .parse::<AgentId>()
        .map_err(|_| ArgError::BadValue {
            flag: "agent",
            value: tail[0].to_string_lossy().into_owned(),
            what: "a canonical agent id",
        })?;
    Ok(if name == "pause" {
        Command::Pause(agent)
    } else {
        Command::Resume(agent)
    })
}

fn lifecycle_control(
    name: &str,
    tail: &[OsString],
    payload: Option<&[OsString]>,
) -> Result<Command, ArgError> {
    if payload.is_some() {
        return Err(ArgError::UnknownFlag(format!("{name} takes no payload")));
    }
    let mut flags = Flags::new(tail);
    let mut agent = None;
    let mut yes = false;
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--agent" if agent.is_none() => {
                agent = Some(flags.parsed::<AgentId>("--agent", "an agent id")?);
            }
            "--yes" if name == "retire" && !yes => yes = true,
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    let agent = required(agent, "--agent")?;
    Ok(if name == "retire" {
        Command::Retire { agent, yes }
    } else {
        Command::Halt(agent)
    })
}

fn goal_command(tail: &[OsString], payload: Option<&[OsString]>) -> Result<Command, ArgError> {
    if tail.first().is_some_and(|word| word == "replace") {
        if tail.len() != 3 || tail[1] != "--request" || tail[2].is_empty() || payload.is_some() {
            return Err(ArgError::UnknownFlag(
                "goal replace requires --request <path>".to_owned(),
            ));
        }
        return Ok(Command::GoalReplace(PathBuf::from(&tail[2])));
    }
    Ok(Command::Goal(Box::new(goal(tail, payload)?)))
}

fn goal(tail: &[OsString], payload: Option<&[OsString]>) -> Result<GoalSetParams, ArgError> {
    let mut flags = Flags::new(tail);
    let (mut agent, mut text, mut usd, mut tokens, mut hop_cap) = (None, None, None, None, None);
    let (mut check_id, mut check_label, mut may_already_pass) = (None, None, false);
    let mut timeout_s = DEFAULT_PREDICATE_TIMEOUT_S;

    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--agent" => agent = Some(flags.parsed::<AgentId>("--agent", "an agent id")?),
            "--text" => text = Some(flags.value("--text")?),
            "--check-id" => check_id = Some(flags.value("--check-id")?),
            "--check-label" => check_label = Some(flags.value("--check-label")?),
            "--check-may-already-pass" => may_already_pass = true,
            "--usd" => usd = Some(flags.parsed::<u64>("--usd", "whole dollars")?),
            "--tokens" => tokens = Some(flags.parsed::<u64>("--tokens", "a whole number")?),
            "--hop-cap" => hop_cap = Some(flags.parsed::<u32>("--hop-cap", "a whole number")?),
            "--timeout-s" => timeout_s = flags.parsed::<u64>("--timeout-s", "seconds")?,
            // Reached only in a flag position: every value-taking flag above has already
            // consumed its value, so a `--help` here is this CLI's and never an operand.
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }

    // Every element converted separately, so a failure is about one argument rather than the whole
    // list. An empty payload is the same mistake as no `--` at all: both mean nothing was asked to
    // decide the goal, and a predicate is what makes `Done` observable (invariant 3).
    let argv = payload.unwrap_or_default();
    if argv.is_empty() {
        return Err(ArgError::NoPredicate);
    }
    let argv: Vec<String> = argv
        .iter()
        .map(|part| {
            part.clone()
                .into_string()
                .map_err(|_| ArgError::NotUtf8("--"))
        })
        .collect::<Result<_, _>>()?;

    Ok(GoalSetParams {
        agent: required(agent, "--agent")?,
        text: required(text, "--text")?,
        predicate: Predicate::Command { argv, timeout_s },
        // Both units optional, both reading the three states — the goal can say what the agent
        // scope can, which is where `--tokens` comes from.
        budget: GoalBudget {
            usd: usd.map(Usd::from_dollars),
            tokens: tokens.map(Tokens::new),
        },
        hop_cap: required(hop_cap, "--hop-cap")?,
        check: check_name(check_id, check_label)?,
        may_already_pass,
    })
}

/// The `{id, label}` a goal records for its check: both flags or neither.
fn check_name(
    id: Option<String>,
    label: Option<String>,
) -> Result<Option<CompletionCheckDeclaration>, ArgError> {
    let (id, label) = match (id, label) {
        (None, None) => return Ok(None),
        (Some(id), Some(label)) => (id, label),
        (Some(_), None) => return Err(ArgError::Requires("--check-id", "--check-label")),
        (None, Some(_)) => return Err(ArgError::Requires("--check-label", "--check-id")),
    };
    match CompletionCheckDeclaration::new(&id, &label) {
        Ok(check) => Ok(Some(check)),
        Err("invalid completion check id") => Err(ArgError::BadValue {
            flag: "--check-id",
            value: id,
            what: "1 to 64 characters of a-z, 0-9, _ and -, starting with a letter",
        }),
        Err(_) => Err(ArgError::BadValue {
            flag: "--check-label",
            value: label,
            what: "1 to 128 bytes with no control characters",
        }),
    }
}

fn why(tail: &[OsString]) -> Result<Command, ArgError> {
    let mut flags = Flags::new(tail);
    let mut agent = None;
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--agent" => agent = Some(flags.parsed::<AgentId>("--agent", "an agent id")?),
            // Reached only in a flag position: every value-taking flag above has already
            // consumed its value, so a `--help` here is this CLI's and never an operand.
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(Command::Why {
        agent: required(agent, "--agent")?,
    })
}

fn ask(tail: &[OsString]) -> Result<Command, ArgError> {
    let mut flags = Flags::new(tail);
    let mut text = None;
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--text" => text = Some(flags.value("--text")?),
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(Command::Ask {
        text: required(text, "--text")?,
    })
}

fn journal_tail(tail: &[OsString]) -> Result<Command, ArgError> {
    let mut flags = Flags::new(tail);
    let (mut from_lsn, mut follow) = (None, false);
    while let Some(arg) = flags.next() {
        match arg.to_string_lossy().as_ref() {
            "--from" => from_lsn = Some(flags.position("--from")?),
            "--follow" => follow = true,
            // Reached only in a flag position: every value-taking flag above has already
            // consumed its value, so a `--help` here is this CLI's and never an operand.
            "--help" | "-h" => return Err(ArgError::HelpRequested),
            other => return Err(ArgError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(Command::Tail {
        params: JournalTailParams { from_lsn },
        follow,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;

    use super::*;

    #[test]
    fn replacement_and_targeted_controls_are_exact_typed_commands() {
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_eq!(
            parse(&args(&["pause", agent])).unwrap().command,
            Command::Pause(agent.parse().unwrap())
        );
        assert_eq!(
            parse(&args(&["resume", agent])).unwrap().command,
            Command::Resume(agent.parse().unwrap())
        );
        assert_eq!(
            parse(&args(&["pause", "--all"])).unwrap().command,
            Command::PauseAll
        );
        assert_eq!(
            parse(&args(&[
                "goal",
                "replace",
                "--request",
                "/tmp/reviewed.json"
            ]))
            .unwrap()
            .command,
            Command::GoalReplace(PathBuf::from("/tmp/reviewed.json"))
        );
        for parts in [
            vec!["pause"],
            vec!["pause", agent, "--all"],
            vec!["resume", "--all"],
            vec!["resume", "not-an-agent"],
            vec!["goal", "replace"],
            vec!["goal", "replace", "--request", "file", "extra"],
            vec!["goal", "replace", "--request", "file", "--", "true"],
        ] {
            assert!(parse(&args(&parts)).is_err(), "{parts:?}");
        }
    }

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[cfg(unix)]
    fn non_unicode_os_string() -> OsString {
        OsString::from_vec(vec![b'a', 0xff, b'b'])
    }

    #[cfg(windows)]
    fn non_unicode_os_string() -> OsString {
        OsString::from_wide(&[u16::from(b'a'), 0xd800, u16::from(b'b')])
    }

    fn an_agent() -> AgentId {
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("an agent id")
    }

    /// The whole `hire` line, and the assertion is on the params that go on the wire rather than on
    /// an intermediate: what a caller can get wrong is the mapping from a flag to a contract field,
    /// and only this comparison sees it.
    #[test]
    fn hire_maps_every_flag_onto_the_contracts_params() {
        let parsed = parse(&args(&[
            "hire",
            "--name",
            "Hector",
            "--role",
            "infra",
            "--department",
            "infra",
            "--provider",
            "claude",
            "--cwd",
            "/repo",
            "--usd-per-day",
            "10",
            "--tokens-per-day",
            "2000",
            "--sandbox",
            "false",
        ]))
        .expect("a well-formed hire");
        let Command::Hire(params) = parsed.command else {
            panic!("a hire")
        };
        assert_eq!(params.name, "Hector");
        assert_eq!(params.role, "infra");
        assert_eq!(params.department, "infra");
        assert_eq!(params.provider, Provider::Claude);
        assert_eq!(params.cwd, PathBuf::from("/repo"));
        assert_eq!(params.budget.usd_per_day, Some(Usd::from_dollars(10)));
        assert_eq!(params.budget.tokens_per_day, Some(Tokens::new(2000)));
        assert_eq!(params.sandbox, Some(false));
        assert!(!parsed.json);
    }

    /// `--max-budget-usd` converts whole dollars into micro-dollars with
    /// checked arithmetic, and refuses zero and the first overflowing value **before** any
    /// `HireParams` exists.
    ///
    /// # What each row is for
    ///
    /// * **Omission is `None`** — no flag composed and no number this CLI invented, the same rule
    ///   `an_omitted_sandbox_flag_stays_absent` pins for its field.
    /// * **`1` is `1_000_000` micro-dollars** — the unit conversion itself, in the smallest case
    ///   where getting the factor wrong is still visible.
    /// * **`18446744073709` is `18_446_744_073_709_000_000`** — the largest whole dollar figure
    ///   that fits, so the arithmetic is exercised at the edge it must still admit rather than
    ///   only in the middle of its range.
    /// * **`18446744073710` is refused** — the very next dollar, and the row this test is named
    ///   for. [`Usd::from_dollars`] *saturates*: writing `max_budget_usd.map(Usd::from_dollars)`
    ///   here compiles, passes the three rows above, and turns an operator's mistyped figure into
    ///   a silent backstop of roughly eighteen trillion dollars recorded for the agent's whole
    ///   life. This row is the only thing that tells the two implementations apart.
    /// * **`0` is refused** — zero has no meaning, and the CLI has the value in hand.
    ///
    /// **Both refusals are asserted as [`ArgError`], not as a rejected call**, because the claim is
    /// that they happen before an `Invocation` or a `HireParams` can be built: `parse` returning
    /// `Err` *is* the observation that nothing was constructed to send.
    ///
    /// [`Usd::from_dollars`]: dudes_types::Usd::from_dollars
    #[test]
    fn max_budget_usd_conversion_is_checked_before_hire_params() {
        fn hire_with(extra: &[&str]) -> Result<Invocation, ArgError> {
            let mut argv = vec![
                "hire",
                "--name",
                "n",
                "--role",
                "r",
                "--department",
                "d",
                "--provider",
                "claude",
                "--cwd",
                "/repo",
                "--usd-per-day",
                "1",
            ];
            argv.extend_from_slice(extra);
            parse(&args(&argv))
        }
        fn budget_of(parsed: Invocation) -> Option<Usd> {
            let Command::Hire(params) = parsed.command else {
                panic!("a hire")
            };
            params.max_budget
        }

        assert_eq!(
            budget_of(hire_with(&[]).expect("a hire with no backstop parses")),
            None,
            "an omitted flag is no backstop, not a figure this CLI chose"
        );
        assert_eq!(
            budget_of(hire_with(&["--max-budget-usd", "1"]).expect("one dollar parses")),
            Some(Usd::from_micros(1_000_000)),
            "whole dollars reach the wire as micro-dollars"
        );
        assert_eq!(
            budget_of(
                hire_with(&["--max-budget-usd", "18446744073709"])
                    .expect("the largest whole dollar figure that fits parses")
            ),
            Some(Usd::from_micros(18_446_744_073_709_000_000)),
            "the top of the range converts rather than being refused"
        );

        for refused in ["18446744073710", "0"] {
            let error = hire_with(&["--max-budget-usd", refused])
                .expect_err("the value must be refused before any params are built");
            assert_eq!(
                error,
                ArgError::BadValue {
                    flag: "--max-budget-usd",
                    value: refused.to_owned(),
                    what: match refused {
                        "0" => "positive whole dollars; omit the flag for no backstop",
                        _ => "whole dollars that fit in micro-dollars (at most 18446744073709)",
                    },
                },
                "`--max-budget-usd {refused}` must not saturate and must not be sent"
            );
        }

        assert_eq!(
            hire_with(&["--max-turns", "0"])
                .expect_err("a turn cap of zero is refused for the same reason"),
            ArgError::BadValue {
                flag: "--max-turns",
                value: "0".to_owned(),
                what: "a positive whole number; omit the flag for no cap",
            }
        );
        let Command::Hire(params) = hire_with(&["--max-turns", "3"])
            .expect("a positive turn cap parses")
            .command
        else {
            panic!("a hire")
        };
        assert_eq!(params.max_turns, Some(3));
    }

    /// `--mcp` repeats in the order typed, and a malformed name is refused before anything is
    /// sent. The daemon refuses a repeat and records the set sorted. Absent is no servers.
    #[test]
    fn hire_mcp_flag_repeats_and_refuses_a_malformed_name() {
        let servers = |extra: &[&str]| {
            let mut line = hire_line("Ada");
            line.extend(extra.iter().map(OsString::from));
            parse(&line).map(|parsed| match parsed.command {
                Command::Hire(params) => params.mcp_servers,
                _ => panic!("a hire"),
            })
        };
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| dudes_types::McpServerName::parse(name).expect("a server name"))
                .collect::<Vec<_>>()
        };
        assert_eq!(servers(&[]).expect("parses"), names(&[]));
        assert_eq!(
            servers(&["--mcp", "notes", "--mcp", "memory"]).expect("parses"),
            names(&["notes", "memory"])
        );
        assert_eq!(
            servers(&["--mcp", "mem__ory"]).expect_err("refused"),
            ArgError::BadValue {
                flag: "--mcp",
                value: "mem__ory".to_owned(),
                what: "a server name from [mcp.servers] in the daemon's configuration",
            }
        );
    }

    /// `--trusted` is the operator's declaration; omitting it is untrusted.
    #[test]
    fn hire_trusted_flag_declares_trust_and_absence_is_untrusted() {
        let trust = |line: &[OsString]| {
            let Command::Hire(params) = parse(line).expect("parses").command else {
                panic!("a hire")
            };
            params.trusted
        };
        let mut declared = hire_line("Ada");
        declared.push("--trusted".into());
        assert!(trust(&declared));
        assert!(!trust(&hire_line("Ada")));
    }

    /// An omitted `--sandbox` is `None` and not `Some(true)`, because the field's
    /// `None` leaves the provider's default alone. A parser that defaulted would send an explicit
    /// value nobody asked for — which the daemon refuses, so every hire from this
    /// CLI would fail.
    #[test]
    fn an_omitted_sandbox_flag_stays_absent() {
        let parsed = parse(&args(&[
            "hire",
            "--name",
            "n",
            "--role",
            "r",
            "--department",
            "d",
            "--provider",
            "claude",
            "--cwd",
            "/repo",
            "--usd-per-day",
            "1",
        ]))
        .expect("a hire");
        let Command::Hire(params) = parsed.command else {
            panic!("a hire")
        };
        assert_eq!(params.sandbox, None);
        assert_eq!(params.budget.tokens_per_day, None);
    }

    /// **The dollar flags are opt-in, and `--hop-cap` is not.** A hire with no `--usd-per-day` and
    /// a goal with no `--usd` both parse, with the ceiling absent rather than defaulted; the same
    /// goal line with no `--hop-cap` is still refused.
    ///
    /// The three cases are one test because the pair is the claim. A blanket "make the flags
    /// optional" edit satisfies the first two and fails the third, and `--hop-cap` is what bounds
    /// every goal in turns once the dollar ceilings may be absent.
    ///
    /// **Controls:** `hire_parses_every_flag` and
    /// `the_predicate_argv_is_taken_whole_and_flags_after_it_are_not_parsed` still assert
    /// `Some(...)` for both flags when they *are* given, so "optional" has not become "ignored".
    #[test]
    fn the_dollar_ceilings_are_optional_and_the_hop_cap_is_not() {
        let parsed = parse(&args(&[
            "hire",
            "--name",
            "n",
            "--role",
            "r",
            "--department",
            "d",
            "--provider",
            "claude",
            "--cwd",
            "/repo",
        ]))
        .expect("a hire with no dollar ceiling parses");
        let Command::Hire(params) = parsed.command else {
            panic!("a hire")
        };
        assert_eq!(
            params.budget.usd_per_day, None,
            "an omitted flag is no ceiling, not a number this CLI invented"
        );

        let parsed = parse(&args(&[
            "goal",
            "--agent",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--text",
            "t",
            "--hop-cap",
            "4",
            "--",
            "/usr/bin/true",
        ]))
        .expect("a goal with no dollar ceiling parses");
        let Command::Goal(params) = parsed.command else {
            panic!("a goal")
        };
        assert_eq!(params.budget.usd, None);
        assert_eq!(params.hop_cap, 4, "and the hop cap it did give is carried");

        assert_eq!(
            parse(&args(&[
                "goal",
                "--agent",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "--text",
                "t",
                "--usd",
                "5",
                "--",
                "/usr/bin/true",
            ]))
            .expect_err("--hop-cap stays required"),
            ArgError::Missing("--hop-cap"),
            "the hop-cap gate bounds every goal in turns, so its flag is the one that stays mandatory"
        );
    }

    /// `--check-id` and `--check-label` name the goal's check together, with the label trimmed as
    /// the daemon trims it, and either alone or a value the daemon would refuse is refused here.
    #[test]
    fn a_goal_names_its_check_with_both_flags_or_neither() {
        let goal_with = |extra: &[&str]| {
            let mut line = vec![
                "goal",
                "--agent",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "--text",
                "t",
                "--hop-cap",
                "4",
            ];
            line.extend_from_slice(extra);
            line.extend_from_slice(&["--", "test", "-f", "CONTRIBUTING.md"]);
            parse(&args(&line)).map(|parsed| {
                let Command::Goal(params) = parsed.command else {
                    panic!("a goal")
                };
                params.check
            })
        };

        assert_eq!(goal_with(&[]), Ok(None), "control: no name");
        assert_eq!(
            goal_with(&[
                "--check-id",
                "contributing",
                "--check-label",
                " CONTRIBUTING.md exists "
            ]),
            Ok(Some(
                CompletionCheckDeclaration::new("contributing", "CONTRIBUTING.md exists").unwrap()
            ))
        );
        assert_eq!(
            goal_with(&["--check-id", "contributing"]),
            Err(ArgError::Requires("--check-id", "--check-label"))
        );
        assert_eq!(
            goal_with(&["--check-label", "CONTRIBUTING.md exists"]),
            Err(ArgError::Requires("--check-label", "--check-id"))
        );
        assert!(matches!(
            goal_with(&["--check-id", "Contributing", "--check-label", "x"]),
            Err(ArgError::BadValue {
                flag: "--check-id",
                ..
            })
        ));
        assert!(matches!(
            goal_with(&["--check-id", "contributing", "--check-label", "   "]),
            Err(ArgError::BadValue {
                flag: "--check-label",
                ..
            })
        ));
    }

    /// **`dudes goal --tokens N` sets the goal's token ceiling**. A parser that wrote a
    /// literal `None` would ignore whatever the operator typed.
    ///
    /// The omitted arm is the control, and it is the sharp one: a parser defaulting to
    /// `Some(Tokens::ZERO)` passes the first assertion and fails it, and that default would be a
    /// ceiling of nothing — refusing every turn — rather than the absence of one.
    #[test]
    fn a_goal_sets_a_token_ceiling_from_the_tokens_flag() {
        let goal_with = |extra: &[&str]| {
            let mut line = vec![
                "goal",
                "--agent",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "--text",
                "t",
                "--hop-cap",
                "4",
            ];
            line.extend_from_slice(extra);
            line.extend_from_slice(&["--", "/usr/bin/true"]);
            let parsed = parse(&args(&line)).expect("a goal");
            let Command::Goal(params) = parsed.command else {
                panic!("a goal")
            };
            params.budget
        };

        assert_eq!(
            goal_with(&["--tokens", "2000"]).tokens,
            Some(Tokens::new(2_000))
        );
        assert_eq!(
            goal_with(&["--tokens", "0"]).tokens,
            Some(Tokens::ZERO),
            "a typed zero is a ceiling of nothing, and reaches the wire as one"
        );
        assert_eq!(
            goal_with(&[]).tokens,
            None,
            "control: omitted is absence, not a defaulted zero"
        );
    }

    /// Everything after `--` is the predicate's argv, verbatim and in order, including the parts
    /// that look like this parser's own flags. Without that arm a predicate could not run a program
    /// that takes `--text`.
    #[test]
    fn the_predicate_argv_is_taken_whole_and_flags_after_it_are_not_parsed() {
        let parsed = parse(&args(&[
            "goal",
            "--agent",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--text",
            "make it pass",
            "--usd",
            "5",
            "--hop-cap",
            "8",
            "--",
            "cargo",
            "test",
            "--text",
            "--json",
        ]))
        .expect("a well-formed goal");
        let Command::Goal(params) = parsed.command else {
            panic!("a goal")
        };
        assert_eq!(params.agent, an_agent());
        assert_eq!(params.text, "make it pass");
        assert_eq!(params.hop_cap, 8);
        assert_eq!(params.budget.usd, Some(Usd::from_dollars(5)));
        assert_eq!(
            params.predicate,
            Predicate::Command {
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--text".to_owned(),
                    "--json".to_owned(),
                ],
                timeout_s: DEFAULT_PREDICATE_TIMEOUT_S,
            }
        );
        assert!(
            !parsed.json,
            "a `--json` after `--` belongs to the predicate, not to this CLI"
        );
    }

    /// `--json` is accepted before and after the command word, and the two agree. The negative arm
    /// is the control: without it a parser that returned `true` always would pass both.
    #[test]
    fn the_json_flag_is_positional_free() {
        for line in [&["--json", "ping"][..], &["ping", "--json"][..]] {
            assert!(parse(&args(line)).expect("parses").json, "{line:?}");
        }
        assert!(!parse(&args(&["ping"])).expect("parses").json);
    }

    /// `quickstart` with no option carries no plan, so the terminal decides whether it asks; any
    /// option makes the plan, and an unknown flag is refused here rather than ignored by startup.
    #[test]
    fn quickstart_options_make_a_plan() {
        let command = |line: &[&str]| parse(&args(line)).map(|invocation| invocation.command);
        assert_eq!(command(&["quickstart"]), Ok(Command::Quickstart(None)));
        assert_eq!(
            command(&["quickstart", "--yes"]),
            Ok(Command::Quickstart(Some(Box::default())))
        );
        assert_eq!(
            command(&["quickstart", "--loud"]),
            Err(ArgError::UnknownFlag("--loud".to_owned()))
        );
        assert_eq!(
            command(&[
                "quickstart",
                "--set",
                "providers.pi",
                "on",
                "--hire",
                "Aaron",
                "--provider",
                "pi",
                "--goal",
                "count the beans",
                "--hop-cap",
                "2",
                "--",
                "/bin/test",
                "-f",
                "beans.txt",
            ]),
            Ok(Command::Quickstart(Some(Box::new(Plan {
                changes: vec![Change::set(Key::Pi, "on").expect("on")],
                check: Some(Check {
                    argv: vec![
                        "/bin/test".to_owned(),
                        "-f".to_owned(),
                        "beans.txt".to_owned()
                    ],
                    timeout_s: 600,
                    hop_cap: 2,
                }),
                enroll: None,
                hire: Some(Hire {
                    name: "Aaron".to_owned(),
                    role: "engineer".to_owned(),
                    provider: Provider::Pi,
                    goal: Some("count the beans".to_owned()),
                    usd_per_day: None,
                    tokens_per_day: None,
                }),
                restart: false,
            }))))
        );
        assert_eq!(
            command(&["quickstart", "--restart"]),
            Ok(Command::Quickstart(Some(Box::new(Plan {
                restart: true,
                ..Plan::default()
            }))))
        );
    }

    /// A hop cap, a check's timeout and a department the daemon would refuse at start are refused
    /// here, before anything is written or stopped.
    #[test]
    fn quickstart_refuses_values_the_daemon_would() {
        let refusal = |line: &[&str]| parse(&args(line)).expect_err("refused");
        for (flag, value, what) in [
            ("--hop-cap", "0", "a whole number above zero"),
            ("--timeout-s", "0", "a whole number above zero"),
            (
                "--department",
                "",
                "1 to 128 characters with no control character",
            ),
        ] {
            assert_eq!(
                refusal(&["quickstart", "--enroll", flag, value, "--", "/usr/bin/true"]),
                ArgError::BadValue {
                    flag,
                    value: value.to_owned(),
                    what,
                },
                "{flag} {value:?}"
            );
        }
    }

    /// An option that only means something beside another is refused alone, naming what it needs.
    #[test]
    fn quickstart_refuses_an_option_without_the_one_it_needs() {
        let refusal = |line: &[&str]| parse(&args(line)).expect_err("refused");
        let check = "a completion check after `--`: `-- <program> [args...]`";
        for (line, flag, needs) in [
            (
                &["quickstart", "--goal-usd", "1"][..],
                "--goal-usd",
                "--enroll",
            ),
            (
                &["quickstart", "--provider", "codex"][..],
                "--provider",
                "--hire <name>",
            ),
            (&["quickstart", "--enroll"][..], "--enroll", check),
            (
                &["quickstart", "--hire", "Kim", "--goal", "ship"][..],
                "--goal",
                check,
            ),
            (
                &["quickstart", "--", "/usr/bin/true"][..],
                "--",
                "--enroll or --goal",
            ),
        ] {
            assert_eq!(refusal(line), ArgError::Requires(flag, needs), "{line:?}");
        }
        assert_eq!(
            refusal(&["--json", "quickstart", "--yes"]),
            ArgError::TextOnly("quickstart with setup options")
        );
    }

    #[test]
    fn journal_position_inputs_accept_raw_and_segment_byte_forms() {
        for (input, expected) in [
            ("1:2048", 1_099_511_629_824),
            ("7", 7),
            ("0:0", 0),
            ("16777215:1099511627775", u64::MAX),
        ] {
            let Command::Tail { params, .. } =
                parse(&args(&["tail", "--from", input])).unwrap().command
            else {
                panic!("tail")
            };
            assert_eq!(params.from_lsn, Some(Lsn(expected)));
            let Command::Memory(MemoryCommand::Read { since_lsn, .. }) =
                parse(&args(&["memory", "read", "--since-lsn", input]))
                    .unwrap()
                    .command
            else {
                panic!("memory read")
            };
            assert_eq!(since_lsn, Some(Lsn(expected)));
        }
        for input in [
            "1:1099511627776",
            "16777216:0",
            "1:2:3",
            "1:",
            ":1",
            "-1:0",
            "1:-1",
            "18446744073709551616",
        ] {
            assert!(parse(&args(&["tail", "--from", input])).is_err(), "{input}");
            assert!(
                parse(&args(&["memory", "read", "--since-lsn", input])).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn invalid_memory_position_shows_a_segment_byte_example() {
        let error = parse(&args(&[
            "memory",
            "read",
            "--since-lsn",
            "segment 1 · byte 2048",
        ]))
        .expect_err("human output needs the input form");
        assert_eq!(
            error.to_string(),
            "`--since-lsn` does not take `segment 1 · byte 2048`: a journal position as \
             segment:byte (for example, 1:2048) or a raw LSN"
        );
    }

    /// `tail` defaults to the tail of the journal and takes a position when given one, and
    /// `--follow` is off unless asked for.
    #[test]
    fn tail_defaults_to_the_tail_and_takes_a_position() {
        let Command::Tail { params, follow } = parse(&args(&["tail"])).expect("parses").command
        else {
            panic!("a tail")
        };
        assert_eq!(params.from_lsn, None);
        assert!(!follow);

        let Command::Tail { params, follow } = parse(&args(&["tail", "--from", "7", "--follow"]))
            .expect("parses")
            .command
        else {
            panic!("a tail")
        };
        assert_eq!(params.from_lsn, Some(Lsn(7)));
        assert!(follow);
    }

    /// The question is the only argument `ask` sends. The agent is deliberately absent from the
    /// command: the daemon derives it from the process's token and peer provenance.
    #[test]
    fn ask_maps_only_text_and_accepts_no_agent_argument() {
        let parsed = parse(&args(&["ask", "--text", "which branch?"])).expect("a well-formed ask");
        assert_eq!(
            parsed.command,
            Command::Ask {
                text: "which branch?".to_owned()
            }
        );
        assert_eq!(
            parse(&args(&["ask", "--agent", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]))
                .expect_err("an ask cannot name an agent"),
            ArgError::UnknownFlag("--agent".to_owned())
        );
    }

    /// Every shape of bad input is an error rather than a default, and each names what was wrong.
    /// The positive control is every other test in this module: these same commands parse when the
    /// flag is right.
    #[test]
    fn bad_input_is_refused_by_shape() {
        let cases: &[(&[&str], ArgError)] = &[
            (&[], ArgError::NoCommand),
            (&["nosuch"], ArgError::UnknownCommand("nosuch".to_owned())),
            (
                &["ping", "--loud"],
                ArgError::UnknownFlag("--loud".to_owned()),
            ),
            (&["why"], ArgError::Missing("--agent")),
            (&["why", "--agent"], ArgError::MissingValue("--agent")),
            (
                &["why", "--agent", "nope"],
                ArgError::BadValue {
                    flag: "--agent",
                    value: "nope".to_owned(),
                    what: "an agent id",
                },
            ),
            (&["hire", "--name", "n"], ArgError::Missing("--role")),
            (
                &[
                    "goal",
                    "--agent",
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "--text",
                    "t",
                    "--usd",
                    "1",
                    "--hop-cap",
                    "1",
                ],
                ArgError::NoPredicate,
            ),
            (
                &[
                    "goal",
                    "--agent",
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "--text",
                    "t",
                    "--usd",
                    "1",
                    "--hop-cap",
                    "1",
                    "--",
                ],
                ArgError::NoPredicate,
            ),
            (
                &["hire", "--provider", "nosuch"],
                ArgError::BadValue {
                    flag: "--provider",
                    value: "nosuch".to_owned(),
                    what: "a known provider",
                },
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse(&args(line)).expect_err("refused"),
                *expected,
                "{line:?}"
            );
        }
    }

    /// A value that is not valid UTF-8 is refused rather than lossily converted, because the wire is
    /// JSON. The control is the path arm: `--cwd` takes the very same bytes and keeps them, since a
    /// path is not text.
    #[test]
    fn non_utf8_text_is_refused_and_a_non_utf8_path_is_kept() {
        let invalid = non_unicode_os_string();

        let mut line = args(&["why", "--agent"]);
        line.push(invalid.clone());
        assert_eq!(
            parse(&line).expect_err("refused"),
            ArgError::NotUtf8("--agent")
        );

        let mut hire_line = args(&[
            "hire",
            "--name",
            "n",
            "--role",
            "r",
            "--department",
            "d",
            "--provider",
            "claude",
            "--usd-per-day",
            "1",
            "--cwd",
        ]);
        hire_line.push(invalid.clone());
        let Command::Hire(params) = parse(&hire_line).expect("a path is not text").command else {
            panic!("a hire")
        };
        assert_eq!(
            params.cwd.as_os_str(),
            invalid.as_os_str(),
            "the bytes reached the params unchanged"
        );
    }

    /// `--help` is recognised wherever it appears, and an ordinary line is not mistaken for it.
    #[test]
    fn help_is_recognised_in_a_flag_position() {
        for line in [
            &["--help"][..],
            &["-h"][..],
            &["help"][..],
            &["quickstart", "--help"][..],
            &["ping", "--help"][..],
            &["ping", "-h"][..],
            &["hire", "--help"][..],
            &["goal", "--help"][..],
            &["ask", "--help"][..],
            &["why", "--help"][..],
            &["tail", "--help"][..],
        ] {
            assert_eq!(
                parse(&args(line)).expect_err("a help request"),
                ArgError::HelpRequested,
                "{line:?}"
            );
        }
        assert!(
            parse(&args(&["ping"])).is_ok(),
            "control: a command is not a help request"
        );
    }

    /// A `goal` line whose predicate argv is `predicate`, with everything else well formed.
    ///
    /// Written once so the help tests below stay readable: what they are about is one token's
    /// position, and eight boilerplate flags around it obscure that.
    fn goal_line<'a>(text: &'a str, predicate: &[&'a str]) -> Vec<OsString> {
        let mut line = vec![
            "goal",
            "--agent",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--text",
            text,
            "--usd",
            "1",
            "--hop-cap",
            "1",
            "--",
        ];
        line.extend_from_slice(predicate);
        args(&line)
    }

    /// A `hire` line whose `--name` value is `name`.
    fn hire_line(name: &str) -> Vec<OsString> {
        args(&[
            "hire",
            "--name",
            name,
            "--role",
            "r",
            "--department",
            "d",
            "--provider",
            "claude",
            "--cwd",
            "/repo",
            "--usd-per-day",
            "1",
        ])
    }

    #[test]
    fn hire_appearance_flags_are_optional_finite_choices() {
        let mut line = hire_line("Ada");
        let Command::Hire(legacy) = parse(&line).unwrap().command else {
            panic!("hire")
        };
        assert_eq!(legacy.character, None);
        assert_eq!(legacy.accent, None);
        line.extend(args(&["--character", "h", "--accent", "sky"]));
        let Command::Hire(chosen) = parse(&line).unwrap().command else {
            panic!("hire")
        };
        assert_eq!(chosen.character, Some(dudes_types::CastCharacter::H));
        assert_eq!(chosen.accent, Some(dudes_types::AccentColour::Sky));
        for (flag, choice) in [
            ("--character", "a"),
            ("--character", "../cast"),
            ("--accent", "#fff"),
            ("--accent", "red"),
        ] {
            let mut invalid = hire_line("Ada");
            invalid.extend(args(&[flag, choice]));
            assert!(parse(&invalid).is_err(), "{flag} {choice}");
        }
    }

    /// **After `--`, a help token is the predicate's operand and the goal is still built.**
    ///
    /// Two of the four lines would print the usage
    /// and exit `0` **having sent nothing** if help were decided by a scan of the whole argv
    /// before the `--` split. A silent no-op reported as success, on the door a loop drives.
    ///
    /// The assertion is on the predicate rather than on the absence of `HelpRequested`, and that is
    /// the control: a parser that had started *rejecting* these lines would satisfy an absence but
    /// not this.
    #[test]
    fn a_help_token_after_the_separator_is_the_predicates_operand() {
        for predicate in [
            &["/usr/bin/make", "--help"][..],
            &["/usr/bin/sort", "-h"][..],
        ] {
            let line = goal_line("t", predicate);
            let Command::Goal(params) = parse(&line)
                .unwrap_or_else(|error| {
                    panic!("{predicate:?} must reach the daemon, not print usage: {error}")
                })
                .command
            else {
                panic!("a goal")
            };
            assert_eq!(
                params.predicate,
                Predicate::Command {
                    argv: predicate.iter().map(|part| (*part).to_owned()).collect(),
                    timeout_s: DEFAULT_PREDICATE_TIMEOUT_S,
                },
                "the help token is the operand it was typed as"
            );
        }
    }

    /// **A help token given as a flag's value is that value**, in every spelling.
    ///
    /// The other two lines from the same measurement. A value is consumed by the flag that takes
    /// it, so it was never in a flag position and never a help request — which a pre-scan of argv
    /// could not know.
    #[test]
    fn a_help_token_given_as_a_flag_value_is_that_value() {
        let Command::Goal(params) = parse(&goal_line("help", &["/usr/bin/true"]))
            .expect("`--text help` is a goal, not a help request")
            .command
        else {
            panic!("a goal")
        };
        assert_eq!(params.text, "help");

        let Command::Ask { text } = parse(&args(&["ask", "--text", "help"]))
            .expect("`--text help` is a question, not a help request")
            .command
        else {
            panic!("an ask")
        };
        assert_eq!(text, "help");

        for name in ["help", "--help", "-h"] {
            let Command::Hire(params) = parse(&hire_line(name))
                .unwrap_or_else(|error| panic!("`--name {name}` is a name: {error}"))
                .command
            else {
                panic!("a hire")
            };
            assert_eq!(params.name, name);
        }
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;
    #[test]
    fn rename_has_exactly_two_utf8_operands() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let parsed = parse(&["rename".into(), id.into(), "Name $with spaces".into()]).unwrap();
        assert_eq!(
            parsed.command,
            Command::Rename(RenameParams {
                agent: id.parse().unwrap(),
                name: "Name $with spaces".to_owned(),
            })
        );
        for args in [
            vec!["rename", id],
            vec!["rename", id, "Name", "extra"],
            vec!["rename", "bad", "Name"],
        ] {
            assert!(parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
    }
}

#[cfg(test)]
mod closing_tests {
    use super::*;

    fn parse_line(line: &[&str]) -> Result<Invocation, ArgError> {
        parse(&line.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn start_takes_no_arguments() {
        assert_eq!(parse_line(&["start"]).unwrap().command, Command::Start);
        assert_eq!(
            parse_line(&["--json", "start"]).unwrap().command,
            Command::Start
        );
        assert!(parse_line(&["start", "--hire", "ada"]).is_err());
    }

    #[test]
    fn stop_defaults_to_closing_and_hard_is_explicit() {
        assert_eq!(parse_line(&["stop"]).unwrap().command, Command::Stop);
        assert_eq!(
            parse_line(&["stop", "--hard"]).unwrap().command,
            Command::StopHard
        );
        assert!(parse_line(&["stop", "--unknown"]).is_err());
        assert_eq!(
            parse_line(&["stop", "--hard", "--help"]).unwrap_err(),
            ArgError::HelpRequested
        );
    }

    #[test]
    fn closing_ack_has_only_an_announcement_cursor() {
        assert_eq!(
            parse_line(&["closing", "ack", "--announcement-lsn", "7"])
                .unwrap()
                .command,
            Command::ClosingAck {
                announcement_lsn: Lsn(7)
            }
        );
        for line in [
            vec!["closing", "ack"],
            vec!["closing", "status"],
            vec!["closing", "ack", "--agent", "01ARZ3NDEKTSV4RRFFQ69G5FAV"],
            vec!["closing", "ack", "--announcement-lsn", "-1"],
        ] {
            assert!(parse_line(&line).is_err(), "{line:?}");
        }
        assert_eq!(
            parse_line(&["closing", "ack", "--help"]).unwrap_err(),
            ArgError::HelpRequested
        );
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn memory_search_preserves_literal_query_and_parses_bounded_operator_options() {
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let invocation = parse(
            &[
                "memory",
                "search",
                " \t🦀 ",
                "--since-lsn",
                "0:7",
                "--limit",
                "100",
                "--agent",
                agent,
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>(),
        )
        .unwrap();
        let Command::Memory(MemoryCommand::Search {
            agent: target,
            options,
        }) = invocation.command
        else {
            panic!("memory search")
        };
        assert_eq!(target, Some(agent.parse().unwrap()));
        assert_eq!(options.query(), " \t🦀 ");
        assert_eq!(options.since_lsn(), Some(Lsn(7)));
        assert_eq!(options.limit(), 100);
        for line in [
            vec!["memory", "search"],
            vec!["memory", "search", ""],
            vec!["memory", "search", "x", "--limit", "0"],
            vec!["memory", "search", "x", "--limit", "101"],
            vec!["memory", "search", "x", "--text", "bad"],
            vec!["memory", "read", "--limit", "2"],
        ] {
            assert!(parse(&line.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
        let Command::Memory(MemoryCommand::Search { agent, options }) = parse(
            &["memory", "search", "literal"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .command
        else {
            panic!("search")
        };
        assert!(agent.is_none());
        assert_eq!(options.limit(), 10);
        assert_eq!(options.since_lsn(), None);
    }

    #[test]
    fn memory_commands_accept_agent_and_operator_forms() {
        for line in [
            vec!["memory", "append", "--text", "remember $this\nexactly"],
            vec!["memory", "read", "--since-lsn", "7"],
            vec!["memory", "read", "--agent", "01ARZ3NDEKTSV4RRFFQ69G5FAV"],
        ] {
            assert!(
                parse(&line.iter().map(OsString::from).collect::<Vec<_>>()).is_ok(),
                "{line:?}"
            );
        }
    }
}

#[cfg(test)]
mod worktree_tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn hire_line(extra: &[&str]) -> Vec<OsString> {
        let mut line = vec![
            "hire",
            "--name",
            "n",
            "--role",
            "r",
            "--department",
            "d",
            "--provider",
            "claude",
            "--cwd",
            "/repo",
        ];
        line.extend_from_slice(extra);
        argv(&line)
    }

    fn hired(extra: &[&str]) -> Result<HireParams, ArgError> {
        parse(&hire_line(extra)).map(|parsed| match parsed.command {
            Command::Hire(params) => *params,
            other => panic!("not a hire: {other:?}"),
        })
    }

    #[test]
    fn a_hire_asks_for_a_worktree_only_with_the_flag_and_names_its_start_with_from() {
        let plain = hired(&["--trusted"]).unwrap();
        assert_eq!((plain.checkout, plain.from), (None, None));
        let own = hired(&["--trusted", "--worktree"]).unwrap();
        assert_eq!(
            own.checkout,
            Some(dudes_proto::rpc::agent::Checkout::Worktree)
        );
        assert_eq!(own.from, None);
        let started = hired(&["--trusted", "--worktree", "--from", ID]).unwrap();
        assert_eq!(started.from, Some(ID.parse().unwrap()));
    }

    #[test]
    fn from_needs_worktree_and_worktree_needs_trusted_and_the_id_must_parse() {
        assert!(matches!(
            hired(&["--trusted", "--from", ID]),
            Err(ArgError::Requires("--from", "--worktree"))
        ));
        assert!(matches!(
            hired(&["--worktree"]),
            Err(ArgError::Requires("--worktree", "--trusted"))
        ));
        assert!(matches!(
            hired(&["--trusted", "--worktree", "--from", "not-an-id"]),
            Err(ArgError::BadValue { flag: "--from", .. })
        ));
    }

    #[test]
    fn worktree_list_and_remove_parse_and_refuse_what_they_do_not_take() {
        let command = |parts: &[&str]| parse(&argv(parts)).map(|parsed| parsed.command);
        assert_eq!(
            command(&["worktree", "list"]).unwrap(),
            Command::Worktree(WorktreeCommand::List)
        );
        assert_eq!(
            command(&["worktree", "remove", ID]).unwrap(),
            Command::Worktree(WorktreeCommand::Remove {
                worktree: ID.parse().unwrap(),
            })
        );
        assert!(matches!(
            command(&["worktree", "remove", "--force", ID]),
            Err(ArgError::UnknownFlag(_))
        ));
        assert!(matches!(
            command(&["worktree", "remove"]),
            Err(ArgError::Missing("<id>"))
        ));
        assert!(matches!(
            command(&["worktree", "remove", "xyz"]),
            Err(ArgError::BadValue { .. })
        ));
        assert!(matches!(
            command(&["worktree", "remove", ID, ID]),
            Err(ArgError::UnknownFlag(_))
        ));
        assert!(command(&["worktree", "list", "extra"]).is_err());
        assert!(matches!(
            command(&["worktree"]),
            Err(ArgError::NoSubcommand("worktree"))
        ));
        assert!(matches!(
            command(&["worktree", "prune"]),
            Err(ArgError::UnknownCommand(_))
        ));
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    fn parsed(words: &[&str]) -> Result<Invocation, ArgError> {
        parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn read_clear_and_replacement_have_distinct_literal_wire_shapes() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        for (words, expected) in [
            (vec!["mcp", id], serde_json::json!({"agent": id})),
            (
                vec!["mcp", id, "--clear"],
                serde_json::json!({"agent": id, "mcp_servers": []}),
            ),
            (
                vec!["mcp", id, "--mcp", "search", "--mcp", "memory", "--json"],
                serde_json::json!({"agent": id, "mcp_servers": ["memory", "search"]}),
            ),
        ] {
            let invocation = parsed(&words).unwrap();
            let Command::Mcp(params) = invocation.command else {
                panic!("an MCP command");
            };
            assert_eq!(serde_json::to_value(params).unwrap(), expected);
            assert_eq!(invocation.json, words.contains(&"--json"));
        }
    }

    #[test]
    fn exclusive_malformed_duplicate_and_extra_arguments_are_refused_locally() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        for words in [
            vec!["mcp"],
            vec!["mcp", "bad"],
            vec!["mcp", id, "--mcp"],
            vec!["mcp", id, "--mcp", "Memory"],
            vec!["mcp", id, "--mcp", "memory", "--mcp", "memory"],
            vec!["mcp", id, "--clear", "--mcp", "memory"],
            vec!["mcp", id, "--provider", "claude"],
            vec!["mcp", id, "--", "ignored"],
        ] {
            assert!(parsed(&words).is_err(), "{words:?}");
        }
        assert_eq!(
            parsed(&["mcp", id, "--clear", "--mcp", "memory"]),
            Err(ArgError::Conflicting("--clear", "--mcp"))
        );
        assert_eq!(parsed(&["mcp", "--help"]), Err(ArgError::HelpRequested));
    }
}
