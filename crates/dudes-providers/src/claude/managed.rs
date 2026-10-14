//! Claude Code, managed mode: the launch of one turn.
//!
//! Vendor flags re-verified against <https://code.claude.com/docs/en/cli-reference> on
//! 2026-08-23: `-p`, `--output-format stream-json`, `--verbose`, `--setting-sources`,
//! `--strict-mcp-config`, `--settings`, `--session-id` ("must be a valid UUID"), `--resume`,
//! `--max-turns`, `--max-budget-usd`, `--dangerously-skip-permissions`. The `--` separator
//! before the task is not in that reference; it is verified by recorded runs of Claude Code
//! 2.1.239 (the fixtures' version), where `-- "<task>"` and a piped stdin behave alike.
//!
//! Both routes are live. A task travels as the last argv element until it passes
//! [`ClaudeConfig::task_argv_max_bytes`], and on stdin above it, because one argv element is
//! bounded by the kernel and stdin is not — that field's doc carries the measurements. The
//! two routes are alternatives: a task is composed into the argv or written to stdin, never
//! both. The separator is kept coupled to the element it introduces because a measurement on
//! 2.1.251 that a `--` with nothing after it **and stdin closed** fails instantly with "Input
//! must be provided either through stdin or as a prompt argument when using --print". Whether
//! a dangling `--` is also refused when stdin carries the prompt is **not measured**, so the
//! coupling is kept for the shape that was measured and claims nothing about the other — and
//! the coupling is not a guarantee: `extra_args` can still compose a dangling separator, as
//! [`ClaudeAdapter::argv`] records. The same probe verified
//! that a task whose first bytes are `-` or `--` is still positional after the separator, so
//! a leading dash is *not* a reason to reach for stdin: size is the only one.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;

use dudes_proto::providers::claude::Settings;
use dudes_types::{AgentId, ModelSelection, Provider, Tier};
use serde_json::{Map, Value};

use crate::adapter::{
    Adapter, AgentCtx, ContextMode, HookInstall, Isolation, ManagedInstall, SessionAcquisition,
    TurnSession, UsageSource,
};
use dudes_types::IsolationMode;

use crate::claude::config::{ClaudeConfig, PermissionMode, SettingSource};
use crate::claude::hooks;
use crate::claude::stream::TurnStream;
use crate::launch::{EnvVar, LaunchError, LaunchSpec};
use crate::mcp::{self, HttpAuth, McpLaunch, McpServer, McpSupport};
use crate::stream::TurnReader;

/// Claude pins: `--session-id` takes a caller-minted UUID and `--resume` a confirmed id.
pub const SESSION_ACQUISITION: SessionAcquisition = SessionAcquisition::Pinned {
    pin: "--session-id",
    resume: "--resume",
};

/// Environment variables never passed to a managed child, matched by exact
/// name. The first seven were measured as markers a running Claude Code sets on its
/// children, present on the daemon when an operator starts it from inside a Claude session.
///
/// `CLAUDE_CODE_SIMPLE` is on the list for its **effect on a child that has it**, which is the
/// only direction anyone has measured; *how* it comes to be on the daemon is not established
/// here and nothing below depends on it. It is documented as equivalent to `--bare` in both
/// directions — the flag's own entry reads "Sets `CLAUDE_CODE_SIMPLE`" — and a measurement on
/// 2.1.251 that a child merely *inheriting* it runs with its hooks suppressed **even when
/// `--settings` names the file explicitly**, broader than the documented "disables
/// auto-discovery of hooks", so passing the settings file is no defence. A managed child that
/// inherited it would be hook-governed in name only, while this adapter still reports
/// [`Tier::HookGoverned`]; and it would reach the vendor's bare state with the hire-time gate
/// that guards the *declared* route into it — [`check`](ClaudeConfig::check) refusing
/// `isolation = "bare"` without an API-key path — never consulted.
///
/// Membership is by name and never by value. The vendor documents "set to `1`", but whether `0`
/// or empty is falsey or merely present-and-truthy was **not** measured, so a value predicate
/// would be keyed on something unknown and would forward the present-but-zero case.
pub const NESTING_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SIMPLE",
];

/// Nesting variables matched by prefix (`CLAUDE_CODE_MESSAGING_*`).
pub const NESTING_PREFIXES: &[&str] = &["CLAUDE_CODE_MESSAGING_"];

/// The fixed isolation flag spellings as declared. Their dynamic source and
/// settings-path values are supplied per agent and therefore cannot live in this static list.
pub const ISOLATION_FLAGS: &[&str] = &["--setting-sources", "--strict-mcp-config", "--settings"];

/// The whole isolation set of `isolation = "bare"`: one flag, which replaces the fixed flags
/// above and their dynamic values rather than joining them. Refused in `extra_args` like the rest
/// of them
/// ([`RESERVED_EXTRA_ARGS`](crate::claude::config::RESERVED_EXTRA_ARGS)), and only reachable
/// through a config [`check`](ClaudeConfig::check) that saw an API-key path.
pub const BARE_FLAG: &[&str] = &["--bare"];

/// The settings file of a managed turn, under the agent's directory. Distinct from the
/// attached-mode `hooks/settings.json` so neither mode picks up the other's keys.
pub const MANAGED_SETTINGS_FILE: &str = "managed-settings.json";

/// The file inside an agent's directory that `CLAUDE_ENV_FILE` names for a managed launch. Claude
/// runs it in its shell before each command, after its own shell snapshot, which is what lets it
/// remove an operator's shell function or alias named `dudes` that would otherwise shadow the
/// daemon's `dudes` on `PATH`.
pub const ENV_GUARD_FILE: &str = "claude-env.sh";

/// The guard's complete content. Constant on purpose: no path and no value is interpolated into
/// it, and it names only `dudes`.
pub const ENV_GUARD: &str = "unset -f dudes 2>/dev/null\nunalias dudes 2>/dev/null\n";

/// One bounded Claude Code 2.1.263 run accepted an empty value and completed. The
/// negative branch remains directly testable through [`ClaudeAdapter::setting_sources`].
const EMPTY_SETTING_SOURCES_SUPPORTED: bool = true;

/// The Claude Code adapter. Owns its configuration; reads nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeAdapter {
    config: ClaudeConfig,
}

impl ClaudeAdapter {
    /// An adapter over one `[providers.claude]` table.
    #[must_use]
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// The configuration this adapter composes launches from.
    #[must_use]
    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }

    /// The content of `managed-settings.json`: `disableAllHooks`,
    /// `autoMemoryEnabled: false`, plus `model` when pinned; no `hooks`, `permissions`,
    /// `enabledPlugins` or `env` key. Under `isolation = inherit` the two isolation keys are
    /// left out — the operator asked for their own hooks and memory — and the file carries
    /// only `model`, so a pinned model is not lost; it is then referenced by `--settings`
    /// alone. Keys are inserted in sorted order, so the bytes are stable whether the map
    /// keeps insertion order or sorts.
    ///
    /// The `model` key is `selection`'s, **not** `self.config.model`'s: the turn's
    /// recorded choice is what its launch material has to describe, or a re-issue after a
    /// configuration change would write a file the journal does not agree with.
    #[must_use]
    pub fn managed_settings(&self, selection: &ModelSelection) -> String {
        let mut extra = Map::new();
        if self.config.isolation == IsolationMode::Default {
            extra.insert("autoMemoryEnabled".into(), Value::Bool(false));
            extra.insert("disableAllHooks".into(), Value::Bool(true));
        }
        if let Some(model) = selection.selector() {
            extra.insert("model".into(), Value::String(model.to_owned()));
        }
        let settings = Settings {
            hooks: BTreeMap::new(),
            extra,
        };
        let mut text = serde_json::to_string(&settings).expect("a settings map serialises");
        text.push('\n');
        text
    }

    /// Resolve the configured override against recorded repository trust.
    ///
    /// `None` means this isolation mode composes no `--setting-sources`. The capability is an
    /// argument so the unsupported-vendor branch stays a pure negative control even
    /// though production's version-specific observation selects `true` today.
    fn setting_sources(
        &self,
        trusted: bool,
        empty_supported: bool,
    ) -> Result<Option<Vec<SettingSource>>, LaunchError> {
        match self.config.isolation {
            IsolationMode::Bare => Ok(None),
            IsolationMode::Inherit if trusted => Ok(None),
            IsolationMode::Inherit => Err(LaunchError::UntrustedRepositorySettings),
            IsolationMode::Default => {
                let sources = self.config.setting_sources.clone().unwrap_or_else(|| {
                    if trusted {
                        vec![SettingSource::Project]
                    } else {
                        Vec::new()
                    }
                });
                if (!trusted && sources.contains(&SettingSource::Project))
                    || (!empty_supported && sources.is_empty())
                {
                    return Err(LaunchError::UntrustedRepositorySettings);
                }
                Ok(Some(sources))
            }
        }
    }

    /// `task` is `Some` when the task travels in the argv and `None` when it goes on stdin
    /// instead. One `Option` ties the separator to the element, so this function never pushes a
    /// `--` that its own task does not follow — the same reason the host derives a piped stdout
    /// and its reader from one `Option` rather than from a field a caller could set alone.
    ///
    /// That does not make a dangling separator unreachable, and the difference was measured
    /// rather than reasoned: `extra_args` is appended immediately before the block below and is
    /// not validated here, so this private composer still relies on `ClaudeConfig::check` having
    /// refused a bare `--` before the adapter was built. The shape is pinned by
    /// `an_extra_args_separator_survives_into_the_stdin_form` and the config-side refusal test.
    ///
    /// Which way a task goes is [`Self::task_on_stdin`], asked once in [`Adapter::managed`].
    fn argv(
        &self,
        ctx: &AgentCtx<'_>,
        task: Option<&str>,
        session: Vec<OsString>,
        selection: &ModelSelection,
        setting_sources: Option<&[SettingSource]>,
    ) -> Result<Vec<OsString>, LaunchError> {
        let mut argv: Vec<OsString> = vec![self.config.command.clone()];
        argv.extend(
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
            ]
            .map(OsString::from),
        );
        let settings = ctx.agent_dir.join(MANAGED_SETTINGS_FILE).into_os_string();
        match self.config.isolation {
            IsolationMode::Default => {
                let sources = setting_sources.expect("default isolation resolves settings sources");
                argv.extend([
                    OsString::from("--setting-sources"),
                    OsString::from(
                        sources
                            .iter()
                            .map(|source| source.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    OsString::from("--strict-mcp-config"),
                    OsString::from("--settings"),
                    settings,
                ]);
            }
            // `--bare` and `inherit` compose no discovery flags. `--settings` survives
            // both because it is explicit rather than discovered: `--bare` is documented as
            // skipping *auto-discovery* (as the CLI reference documents),
            // and on 2.1.251 the flag's own help names `--settings` among the ways to give a
            // bare session context and reads "Anthropic auth is strictly ANTHROPIC_API_KEY or
            // apiKeyHelper via --settings" (`claude --help`, 2026-08-30). That the `model` key
            // *inside* that file still pins the model on a bare turn is **not verified** — it
            // needs an authenticated turn — and it is the half that would fail silently: a
            // pinned model ignored, with no diagnostic.
            IsolationMode::Bare | IsolationMode::Inherit => {
                if self.config.isolation == IsolationMode::Bare {
                    argv.extend(BARE_FLAG.iter().map(OsString::from));
                }
                if selection.selector().is_some() {
                    argv.extend(["--settings".into(), settings]);
                }
            }
        }
        argv.extend(session);
        // `--mcp-config` takes every following element that is not an option, so what follows the
        // value is always an option, the `--` before an argv task, or the end of the argv.
        if !ctx.mcp_servers.is_empty() {
            argv.extend([
                OsString::from("--mcp-config"),
                OsString::from(mcp_config(ctx.agent, ctx.cwd, ctx.mcp_servers)?),
            ]);
        }
        if let Some(turns) = ctx.max_turns {
            argv.extend(["--max-turns".to_owned(), turns.to_string()].map(OsString::from));
        }
        // **Not a spend ceiling, and the distinction is the operator's.** The figure the vendor
        // checks this against is one it computes locally from published list prices, so what the
        // flag actually does is throttle token volume inside a single turn. It is neither billed
        // against nor reconciled with what the account is charged. Dudes' spend ceilings are gate
        // 5's — goal, agent and floor — and they are checked before a turn is issued and journaled
        // on `Record::BudgetDecision`.
        if let Some(usd) = ctx.max_budget {
            argv.extend(["--max-budget-usd".to_owned(), usd.to_decimal()].map(OsString::from));
        }
        if self.config.permission_mode == PermissionMode::Bypass {
            argv.push("--dangerously-skip-permissions".into());
        }
        append_list(
            &mut argv,
            "--disallowedTools",
            &self.config.disallowed_tools,
        );
        append_list(&mut argv, "--allowedTools", &self.config.allowed_tools);
        append_list(&mut argv, "--tools", &self.config.tools);
        argv.extend(self.config.extra_args.iter().cloned());
        if let Some(task) = task {
            argv.push("--".into());
            argv.push(task.into());
        }
        Ok(argv)
    }

    /// Whether this task is delivered on stdin rather than as the last argv element: strictly
    /// larger than [`ClaudeConfig::task_argv_max_bytes`], so a task of exactly the ceiling is
    /// still argv. Bytes, not characters — an argv element is bytes to `execve`, and a task of
    /// `n` multi-byte characters is more than `n` bytes on the way to the kernel.
    fn task_on_stdin(&self, task: &str) -> bool {
        task.len() > self.config.task_argv_max_bytes
    }
}

/// The names a program server's `env` map sets to empty besides Claude's billing names and the
/// secrets of the agent's other servers. A program server inherits Claude's whole environment, so
/// without these it would hold the agent's credential and socket, its mailbox, and Claude's own
/// messaging pair, and could act as the agent. Besides the Dudes and messaging names, the list holds
/// the credentials Claude reads for its sign-in token, custom auth headers, mTLS client key and the AWS,
/// Google and Azure routes: secrets and the files that hold them, never the routing choices (region, project, endpoint) that
/// only select a destination.
const BLANKED_FOR_PROGRAMS: &[&str] = &[
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "DUDES_MAILBOX",
    "DUDES_SOCK",
    "DUDES_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
    "AWS_ACCESS_KEY_ID",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AZURE_CLIENT_CERTIFICATE_PASSWORD",
    "AZURE_CLIENT_CERTIFICATE_PATH",
    "AZURE_CLIENT_SECRET",
    "AZURE_FEDERATED_TOKEN_FILE",
    "AZURE_PASSWORD",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "IDENTITY_HEADER",
    "MSI_SECRET",
];

/// The one `--mcp-config` value of a launch: every admitted server, keys sorted.
///
/// An http server's credential is a `${ALIAS}` reference, which Claude expands from its own
/// environment, so the value never enters the argv. A program server's `env` map sets
/// [`BLANKED_FOR_PROGRAMS`], Claude's billing names and every secret of the agent's other servers
/// to empty, its own secrets by reference, and `DUDES_AGENT_ID` as a label. Blanking narrows what
/// the server inherits; it is not a boundary, because a process of the same user can read
/// Claude's environment.
fn mcp_config(agent: AgentId, cwd: &Path, servers: &[McpLaunch]) -> Result<String, LaunchError> {
    let reference = |name: &str| format!("${{{name}}}");
    let entries: BTreeMap<&str, BTreeMap<&str, Value>> = servers
        .iter()
        .map(|launch| -> Result<_, LaunchError> {
            let mut entry = BTreeMap::new();
            match &launch.server {
                McpServer::Http(http) => {
                    let headers: BTreeMap<String, String> = match &http.auth {
                        HttpAuth::None => BTreeMap::new(),
                        HttpAuth::Bearer(_) => BTreeMap::from([(
                            "Authorization".to_owned(),
                            format!("Bearer {}", reference(&mcp::bearer_alias(&launch.name))),
                        )]),
                        HttpAuth::Headers(headers) => headers
                            .keys()
                            .enumerate()
                            .map(|(position, header)| {
                                let alias = mcp::header_alias(&launch.name, position);
                                (header.clone(), reference(&alias))
                            })
                            .collect(),
                    };
                    if !headers.is_empty() {
                        entry.insert("headers", serde_json::json!(headers));
                    }
                    entry.insert("type", Value::from("http"));
                    entry.insert("url", Value::from(http.url.as_str()));
                }
                McpServer::Program(program) => {
                    let own: BTreeSet<&str> = program
                        .secret_env
                        .iter()
                        .map(mcp::VariableName::as_str)
                        .collect();
                    let others = servers
                        .iter()
                        .filter(|other| other.name != launch.name)
                        .flat_map(|other| &other.env)
                        .map(|var| var.name().to_string_lossy().into_owned());
                    let mut env: BTreeMap<String, String> = BLANKED_FOR_PROGRAMS
                        .iter()
                        .chain(crate::claude::config::BILLING_ENV_VARS)
                        .map(|name| (*name).to_owned())
                        .chain(others)
                        .filter(|name| !own.contains(name.as_str()))
                        .map(|name| (name, String::new()))
                        .collect();
                    env.extend(own.iter().map(|name| ((*name).to_owned(), reference(name))));
                    env.insert("DUDES_AGENT_ID".to_owned(), agent.to_string());
                    let args = program
                        .args
                        .iter()
                        .map(|arg| {
                            if arg == mcp::AGENT_DIR_ARG {
                                cwd.to_str()
                                    .map(str::to_owned)
                                    .ok_or(LaunchError::McpAgentDirEncoding)
                            } else {
                                Ok(arg.clone())
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    entry.insert("args", Value::from(args));
                    entry.insert("command", Value::from(program.command.as_str()));
                    entry.insert("env", serde_json::json!(env));
                    entry.insert("type", Value::from("stdio"));
                }
            }
            Ok((launch.name.as_str(), entry))
        })
        .collect::<Result<_, _>>()?;
    Ok(
        serde_json::to_string(&BTreeMap::from([("mcpServers", entries)]))
            .expect("an MCP configuration of strings serialises"),
    )
}

/// The child's environment: the daemon's minus [`NESTING_VARS`] and [`NESTING_PREFIXES`],
/// everything else verbatim, sorted by name so the result is byte-stable. `DUDES_SOCK` and
/// `DUDES_TOKEN` are among the pass-through: they were once scrubbed, but
/// the supervisor now sets both on every agent child,
/// managed and attached alike, and an adapter must not remove what the supervisor deliberately
/// set. Nothing is added: this crate mints no credential and reads no environment of its own.
///
/// Windows environment names are case-insensitive, so both nesting comparisons fold ASCII case
/// there. The nesting names are ASCII, making bytewise ASCII folding sufficient; other targets
/// retain exact-name and byte-prefix matching because their environment names distinguish case.
#[must_use]
pub fn scrub_env(daemon_env: &[EnvVar]) -> Vec<EnvVar> {
    let dropped = |key: &OsStr| {
        let bytes = key.as_encoded_bytes();
        let exact = |name: &str| {
            if cfg!(windows) {
                bytes.eq_ignore_ascii_case(name.as_bytes())
            } else {
                key == OsStr::new(name)
            }
        };
        let prefix = |prefix: &str| {
            if cfg!(windows) {
                bytes
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
            } else {
                bytes.starts_with(prefix.as_bytes())
            }
        };
        NESTING_VARS.iter().any(|name| exact(name))
            || NESTING_PREFIXES.iter().any(|name| prefix(name))
    };
    let mut env: Vec<_> = daemon_env
        .iter()
        .filter(|var| !dropped(var.name()))
        .cloned()
        .collect();
    env.sort();
    env
}

/// `env` with `CLAUDE_ENV_FILE` naming this agent's guard file, replacing any value the daemon
/// inherited. Unix only: the file is a shell script and Windows has no agent mail to protect.
fn with_env_guard(mut env: Vec<EnvVar>, agent_dir: &Path) -> Vec<EnvVar> {
    if cfg!(unix) {
        env.retain(|var| var.name() != "CLAUDE_ENV_FILE");
        env.push(EnvVar::new(
            "CLAUDE_ENV_FILE",
            agent_dir.join(ENV_GUARD_FILE),
        ));
        env.sort();
    }
    env
}

impl Adapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &OsStr {
        &self.config.command
    }

    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }

    /// `Flags` under the default isolation and under `bare`; `None` for an agent whose
    /// operator chose `inherit`, so the hire log and the floor say what the turn will run.
    fn isolation(&self) -> Isolation {
        match self.config.isolation {
            IsolationMode::Default => Isolation::Flags(ISOLATION_FLAGS),
            IsolationMode::Bare => Isolation::Flags(BARE_FLAG),
            IsolationMode::Inherit => Isolation::None,
        }
    }

    fn context_mode(&self, mode: dudes_types::Mode) -> ContextMode {
        match mode {
            dudes_types::Mode::Managed => ContextMode::ClaudeManaged(self.config.isolation),
            dudes_types::Mode::Attached => ContextMode::ClaudeAttached,
        }
    }

    fn instruction_manifest(
        &self,
        ctx: &AgentCtx<'_>,
        mode: dudes_types::Mode,
        selection: &ModelSelection,
    ) -> crate::instructions::InstructionDeclaration {
        use crate::instructions::{ClaudeSources, DeclarationMode, claude_manifest};

        if mode == dudes_types::Mode::Attached {
            return claude_manifest(
                ctx,
                DeclarationMode::ClaudeAttached,
                ClaudeSources::Inherit,
                &[],
                crate::instructions::validate_path(ctx.agent_dir).map(|()| {
                    Some(
                        hooks::settings_path(ctx)
                            .strip_prefix(ctx.agent_dir)
                            .expect("hook settings are agent-owned")
                            .to_owned(),
                    )
                }),
            );
        }
        let resolved = if self.config.isolation == IsolationMode::Default
            && self.config.setting_sources.as_ref().is_some_and(|sources| {
                sources.len() > dudes_proto::instruction::MAX_INSTRUCTION_INPUTS
            }) {
            None
        } else {
            Some(self.setting_sources(ctx.trusted, EMPTY_SETTING_SOURCES_SUPPORTED))
        };
        let sources = match &resolved {
            Some(Ok(Some(selected))) => ClaudeSources::Selected(selected),
            Some(Ok(None)) if self.config.isolation == IsolationMode::Bare => ClaudeSources::Bare,
            Some(Ok(None)) => ClaudeSources::Inherit,
            Some(Err(_)) => ClaudeSources::Refused,
            None => ClaudeSources::Unresolved,
        };
        let settings = (self.config.isolation == IsolationMode::Default
            || selection.selector().is_some())
        .then(|| MANAGED_SETTINGS_FILE.into());
        claude_manifest(
            ctx,
            DeclarationMode::ClaudeManaged(self.config.isolation),
            sources,
            &self.config.extra_args,
            Ok(settings),
        )
    }

    /// The configured selector, copied rather than interpreted: an unset `model` is the provider's
    /// own default and is recorded as such, which is a different fact from a turn issued before
    /// the field existed. This adapter resolves no alias and knows no catalogue.
    fn model_selection(&self) -> ModelSelection {
        ModelSelection {
            selector: self.config.model.clone(),
        }
    }

    /// The task goes one of two ways and the choice is made once, here: in the argv after `--`,
    /// or on stdin with the argv ending at the last flag.
    ///
    /// Invariant 6 (no REPL typing) is not in play, and the reason is structural rather than
    /// circumstantial. [`LaunchSpec::stdin`] is an `Option<Vec<u8>>` with two states and no
    /// third — the null device, or these bytes written and the pipe closed — so a *second*
    /// write is not a thing this crate can describe, let alone perform. What invariant 6
    /// forbids is a message body driving a live REPL's keyboard, and that shape is
    /// unrepresentable in the type the launch is carried in. It is worth putting it that way
    /// round: "there is no PTY on this path" is true today and stops being an argument the day
    /// one is added, whereas the type's two states do not. Delivery here is the same
    /// typed-parts rule as the argv (invariant 7), down a descriptor instead of an array.
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError> {
        if task.is_empty() {
            return Err(LaunchError::EmptyTask);
        }
        if let Some(offset) = task.find('\0') {
            return Err(LaunchError::TaskContainsNul { offset });
        }
        let setting_sources = self.setting_sources(ctx.trusted, EMPTY_SETTING_SOURCES_SUPPORTED)?;
        let session = self.session_acquisition().argv(session)?;
        let on_stdin = self.task_on_stdin(task);
        Ok(LaunchSpec {
            argv: self.argv(
                ctx,
                (!on_stdin).then_some(task),
                session,
                selection,
                setting_sources.as_deref(),
            )?,
            env: mcp::with_secrets(
                with_env_guard(self.child_env(ctx.daemon_env), ctx.agent_dir),
                ctx.mcp_servers,
            ),
            cwd: ctx.cwd.to_path_buf(),
            stdin: if on_stdin {
                crate::launch::LaunchInput::WriteThenClose(task.as_bytes().to_vec())
            } else {
                crate::launch::LaunchInput::Null
            },
        })
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    /// One file, unless the launch references none (`inherit` without a pinned model). Whether a
    /// model is pinned is `selection`'s answer and not the configuration's, so the
    /// file exists exactly where [`Self::argv`] composed a `--settings` for it.
    fn managed_install(&self, ctx: &AgentCtx<'_>, selection: &ModelSelection) -> ManagedInstall {
        let referenced =
            self.config.isolation == IsolationMode::Default || selection.selector().is_some();
        let mut files: Vec<_> = referenced
            .then(|| {
                (
                    ctx.agent_dir.join(MANAGED_SETTINGS_FILE),
                    self.managed_settings(selection),
                )
            })
            .into_iter()
            .collect();
        if cfg!(unix) {
            files.push((ctx.agent_dir.join(ENV_GUARD_FILE), ENV_GUARD.to_owned()));
        }
        ManagedInstall {
            directories: Vec::new(),
            files,
        }
    }

    fn attached(&self, ctx: &AgentCtx<'_>, session: &TurnSession) -> Option<LaunchSpec> {
        hooks::compose_attached(&self.config.command, ctx, session)
    }

    fn install_hooks(&self, ctx: &AgentCtx<'_>, hook_command: &Path) -> HookInstall {
        hooks::install(ctx, hook_command)
    }

    fn wake_sentinel(&self) -> Option<&'static str> {
        Some(hooks::WAKE_SENTINEL)
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    /// Claude's sandbox on-switch is a settings key, not a flag; none until the settings
    /// file's content is extended by a decision.
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        crate::claude::config::BILLING_ENV_VARS
    }

    fn sign_in_check(&self) -> Option<&'static [&'static str]> {
        Some(SIGN_IN_STATUS)
    }

    fn child_env(&self, daemon_env: &[EnvVar]) -> Vec<EnvVar> {
        scrub_env(daemon_env)
    }

    /// Program and http servers under the default isolation, where `--strict-mcp-config` keeps
    /// every other source out. A program server stays in the turn's process group: Claude starts
    /// it as its own child and ends it on exit, and the group kill reaches it. The other
    /// isolations compose no strict flag, so they load none.
    fn mcp_support(&self) -> McpSupport {
        if self.config.isolation == IsolationMode::Default {
            McpSupport::Program
        } else {
            McpSupport::None
        }
    }
}

/// `claude auth status`, which exits 0 when signed in and 1 when it finds no login
/// (claude 2.1.284).
pub const SIGN_IN_STATUS: &[&str] = &["auth", "status"];

/// `claude auth login`, the vendor's own interactive sign-in.
pub const LOGIN: &[&str] = &["auth", "login"];

/// Append one variadic tool flag only when it has values; each value remains one typed argv part.
fn append_list(argv: &mut Vec<OsString>, flag: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    argv.push(flag.into());
    argv.extend(values.iter().map(OsString::from));
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use dudes_types::{AgentId, SessionId, Usd};

    use super::*;

    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const AGENT_DIR: &str = "/tmp/dudes-data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// The guard file and `CLAUDE_ENV_FILE` are pinned by their own tests; the older assertions
    /// below are about everything else.
    fn minus_guard_env(mut env: Vec<EnvVar>) -> Vec<EnvVar> {
        env.retain(|var| var.name() != "CLAUDE_ENV_FILE");
        env
    }

    fn minus_guard_files(mut files: Vec<(PathBuf, String)>) -> Vec<(PathBuf, String)> {
        files.retain(|(path, _)| !path.ends_with(ENV_GUARD_FILE));
        files
    }

    /// The settings path exactly as the adapter builds it (`agent_dir.join(..)`), so the
    /// expectation carries the platform's separator: `/` on Unix, `\` on Windows.
    fn settings() -> OsString {
        Path::new(AGENT_DIR)
            .join(MANAGED_SETTINGS_FILE)
            .into_os_string()
    }

    fn ctx(env: &[EnvVar]) -> AgentCtx<'_> {
        AgentCtx {
            agent: AgentId::parse(AGENT).unwrap(),
            cwd: Path::new("/tmp/dudes-fixture"),
            agent_dir: Path::new(AGENT_DIR),
            daemon_env: env,
            sandbox: None,
            trusted: false,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        }
    }

    fn trusted_ctx(env: &[EnvVar]) -> AgentCtx<'_> {
        AgentCtx {
            trusted: true,
            ..ctx(env)
        }
    }

    fn os(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn first() -> TurnSession {
        TurnSession::First(SessionId::from(UUID))
    }

    /// The model these tests pin when they pin one. A constant, so the config that names it and
    /// the selection composed under it cannot drift apart by a typo and pass anyway.
    const MODEL: &str = "claude-haiku-4-5";

    /// The provider's own default: a selection, deliberately made, naming no model.
    fn none() -> ModelSelection {
        ModelSelection::provider_default()
    }

    /// A selection naming one model. Written out rather than taken from the adapter's own
    /// configuration: what these tests state is that the launch is composed from **the selection
    /// it is given**, and an argument the subject supplies for itself could not say
    /// that.
    fn pin(model: &str) -> ModelSelection {
        ModelSelection::named(model)
    }

    /// refusal is keyed on the task itself and reports the byte offset,
    /// including when multi-byte text precedes the NUL.
    #[test]
    fn a_task_with_nul_is_refused_with_its_byte_offset() {
        for adapter in [
            ClaudeAdapter::default(),
            ClaudeAdapter::new(ClaudeConfig {
                task_argv_max_bytes: 1,
                ..ClaudeConfig::default()
            }),
        ] {
            let error = adapter
                .managed(&ctx(&[]), "α\0ship", &first(), &none())
                .expect_err("a task containing NUL is not a launch on argv or stdin");

            assert_eq!(
                error,
                crate::launch::LaunchError::TaskContainsNul { offset: 2 }
            );
        }
    }

    /// Invariant 7 gives a turn typed command parts; an empty task carries no work and must be
    /// refused before the adapter can return any of those parts.
    #[test]
    fn an_empty_task_is_refused_before_invariant_7_parts_exist() {
        let error = ClaudeAdapter::default()
            .managed(&ctx(&[]), "", &first(), &none())
            .expect_err("an empty task is not a launch");

        assert_eq!(error, crate::launch::LaunchError::EmptyTask);
    }

    const HEAD: [&str; 6] = [
        "claude",
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
    ];
    fn isolation(source: &str) -> Vec<OsString> {
        let mut v = os(&[
            "--setting-sources",
            source,
            "--strict-mcp-config",
            "--settings",
        ]);
        v.push(settings());
        v
    }

    /// The isolation argv, byte for byte: `TurnSession::First` renders `--session-id <uuid>` and
    /// `Resume` renders `--resume <id>`, the task is the single last element after `--`, stdin
    /// is the null device and cwd is the hire's. Only the rendering is asserted here; which
    /// variant a turn gets is the supervisor's.
    ///
    /// It is also the untrusted half of the rule that an agent with no declared server composes
    /// this argv byte for byte. Mutant: compose `--mcp-config` with an empty server map.
    #[test]
    fn claude_untrusted_argv_is_byte_stable() {
        let adapter = ClaudeAdapter::default();
        let spec = adapter
            .managed(&ctx(&[]), "echo hello", &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        let mut want = os(&HEAD);
        want.extend(isolation(""));
        want.extend(os(&[
            "--session-id",
            UUID,
            "--dangerously-skip-permissions",
            "--",
            "echo hello",
        ]));
        assert_eq!(spec.argv, want);
        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
        assert_eq!(spec.cwd, Path::new("/tmp/dudes-fixture"));

        let resumed = adapter
            .managed(
                &ctx(&[]),
                "echo hello",
                &TurnSession::Resume("s-2".into()),
                &none(),
            )
            .expect("a non-empty task without NUL is a launch");
        let mut want = os(&HEAD);
        want.extend(isolation(""));
        want.extend(os(&[
            "--resume",
            "s-2",
            "--dangerously-skip-permissions",
            "--",
            "echo hello",
        ]));
        assert_eq!(resumed.argv, want);
    }

    #[test]
    fn claude_refuses_a_turn_that_announces_no_session() {
        assert_eq!(
            ClaudeAdapter::default().managed(
                &ctx(&[]),
                "echo hello",
                &TurnSession::Assigned,
                &none()
            ),
            Err(LaunchError::SessionModeMismatch)
        );
    }

    /// The opposite polarity: the same configured default, changed only to recorded trust,
    /// carries the project source. Without this control an adapter that always emitted empty would
    /// satisfy the untrusted fixture above. It is the trusted half of the no-server check, for the same
    /// mutant.
    #[test]
    fn claude_trusted_argv_still_carries_project() {
        let spec = ClaudeAdapter::default()
            .managed(&trusted_ctx(&[]), "t", &first(), &none())
            .expect("recorded trust admits the project default");
        let mut want = os(&HEAD);
        want.extend(isolation("project"));
        want.extend(os(&[
            "--session-id",
            UUID,
            "--dangerously-skip-permissions",
            "--",
            "t",
        ]));
        assert_eq!(spec.argv, want);
        assert_eq!(
            ClaudeAdapter::default().isolation(),
            Isolation::Flags(&["--setting-sources", "--strict-mcp-config", "--settings"])
        );
    }

    fn declared_servers(names: &[&str]) -> Vec<McpLaunch> {
        crate::mcp::fixture::servers(names)
    }

    /// The `--mcp-config` value for the fixture's `memory` server alone.
    const MEMORY_ONLY: &str = r#"{"mcpServers":{"memory":{"headers":{"Authorization":"${DUDES_MCP_MEMORY_HEADER_0}"},"type":"http","url":"https://memory.example.com/mcp"}}}"#;

    /// One declared server adds exactly `--mcp-config <value>` after the session flags,
    /// trusted and untrusted, with the task in the argv and on stdin, and what follows the value
    /// is an option. Mutants: the pair moved after the task separator, a value that is not this
    /// literal, or a second pair.
    #[test]
    fn one_declared_server_adds_one_value_right_after_the_session() {
        let servers = declared_servers(&["memory"]);
        let long = "t".repeat(ClaudeConfig::default().task_argv_max_bytes + 1);
        for trusted in [false, true] {
            let context = AgentCtx {
                trusted,
                mcp_servers: &servers,
                ..ctx(&[])
            };
            for task in ["t", long.as_str()] {
                let argv = ClaudeAdapter::default()
                    .managed(&context, task, &first(), &none())
                    .expect("a declared server is a launch")
                    .argv;
                let mut want = isolation(if trusted { "project" } else { "" });
                want.extend(os(&["--session-id", UUID, "--mcp-config", MEMORY_ONLY]));
                let at = argv
                    .iter()
                    .position(|part| part == "--setting-sources")
                    .expect("default isolation composes its sources");
                assert_eq!(argv[at..at + want.len()], want[..]);
                assert!(
                    argv.get(at + want.len())
                        .is_none_or(|next| next.to_string_lossy().starts_with('-')),
                    "the variadic flag must be followed by an option or nothing"
                );
                assert_eq!(
                    argv.iter().filter(|part| *part == "--mcp-config").count(),
                    1
                );
            }
        }
        let with = AgentCtx {
            mcp_servers: &servers,
            ..ctx(&[])
        };
        assert_eq!(
            ClaudeAdapter::default().managed_install(&with, &none()),
            ClaudeAdapter::default().managed_install(&ctx(&[]), &none()),
            "managed-settings.json does not change with a declared server"
        );
    }

    /// A program server's `env` map blanks exactly the agent's credential, socket and
    /// mailbox, Claude's messaging pair and billing names and every other server's secret, sets its
    /// own secret by reference and the agent label as a literal, and holds nothing else. Mutant:
    /// drop any one blank, and the comparison fails.
    #[test]
    fn a_program_server_env_blanks_what_it_does_not_own() {
        let servers = declared_servers(&["memory", "notes", "search"]);
        let context = AgentCtx {
            trusted: true,
            mcp_servers: &servers,
            ..ctx(&[])
        };
        let argv = ClaudeAdapter::default()
            .managed(&context, "t", &first(), &none())
            .expect("a launch")
            .argv;
        let at = argv
            .iter()
            .position(|part| part == "--mcp-config")
            .expect("composed");
        let value: Value =
            serde_json::from_str(&argv[at + 1].to_string_lossy()).expect("one JSON value");
        assert_eq!(
            value["mcpServers"]["notes"],
            serde_json::json!({
                "args": ["--stdio"],
                "command": "/opt/homebrew/bin/notes-mcp",
                "type": "stdio",
                "env": {
                    "ANTHROPIC_API_KEY": "",
                    "ANTHROPIC_AUTH_TOKEN": "",
                    "ANTHROPIC_AWS_API_KEY": "",
                    "ANTHROPIC_CUSTOM_HEADERS": "",
                    "ANTHROPIC_FOUNDRY_API_KEY": "",
                    "ANTHROPIC_FOUNDRY_AUTH_TOKEN": "",
                    "ANTHROPIC_OAUTH_TOKEN": "",
                    "AWS_BEARER_TOKEN_BEDROCK": "",
                    "AWS_ACCESS_KEY_ID": "",
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN": "",
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE": "",
                    "AWS_SECRET_ACCESS_KEY": "",
                    "AWS_SESSION_TOKEN": "",
                    "AWS_WEB_IDENTITY_TOKEN_FILE": "",
                    "AZURE_CLIENT_CERTIFICATE_PASSWORD": "",
                    "AZURE_CLIENT_CERTIFICATE_PATH": "",
                    "AZURE_CLIENT_SECRET": "",
                    "AZURE_FEDERATED_TOKEN_FILE": "",
                    "AZURE_PASSWORD": "",
                    "CLAUDE_CODE_OAUTH_TOKEN": "",
                    "GOOGLE_APPLICATION_CREDENTIALS": "",
                    "IDENTITY_HEADER": "",
                    "MSI_SECRET": "",
                    "CLAUDE_CODE_CLIENT_KEY": "",
                    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE": "",
                    "CLAUDE_CODE_MESSAGING_SOCKET": "",
                    "CLAUDE_CODE_MESSAGING_TOKEN": "",
                    "DUDES_AGENT_ID": AGENT,
                    "DUDES_MAILBOX": "",
                    "DUDES_MCP_MEMORY_HEADER_0": "",
                    "DUDES_MCP_SEARCH_BEARER": "",
                    "DUDES_SOCK": "",
                    "DUDES_TOKEN": "",
                    "NOTES_API_KEY": "${NOTES_API_KEY}",
                },
            })
        );
        assert_eq!(
            value["mcpServers"]["search"],
            serde_json::json!({
                "headers": {"Authorization": "Bearer ${DUDES_MCP_SEARCH_BEARER}"},
                "type": "http",
                "url": "http://127.0.0.1:8931/mcp",
            })
        );
    }

    fn folder_servers() -> Vec<McpLaunch> {
        let file = "[notes]\ncommand = \"notes-mcp\"\nargs = [\"--root\", \"{agent_dir}\", \"{agent_dir}\"]\n";
        crate::mcp::LoadedServers::new(&crate::mcp::fixture::declared(file), &[], |command| {
            Some(format!("/opt/homebrew/bin/{command}"))
        })
        .admit(
            &crate::mcp::fixture::names(&["notes"]),
            crate::mcp::Rules {
                support: McpSupport::Program,
                default_isolation: true,
                trusted: true,
            },
        )
        .expect("admits")
    }

    /// A whole `{agent_dir}` entry becomes the agent's recorded folder at each launch, in every
    /// position, and every other entry is untouched. Mutant: stop resolving, and the token
    /// reaches the server.
    #[test]
    fn a_program_server_argument_names_the_agents_folder() {
        let servers = folder_servers();
        let context = AgentCtx {
            trusted: true,
            mcp_servers: &servers,
            ..ctx(&[])
        };
        let argv = ClaudeAdapter::default()
            .managed(&context, "t", &first(), &none())
            .expect("a launch")
            .argv;
        let at = argv.iter().position(|part| part == "--mcp-config").unwrap();
        let value: Value = serde_json::from_str(&argv[at + 1].to_string_lossy()).unwrap();
        assert_eq!(
            value["mcpServers"]["notes"]["args"],
            serde_json::json!(["--root", "/tmp/dudes-fixture", "/tmp/dudes-fixture"])
        );
    }

    /// A folder that is not UTF-8 refuses the launch rather than naming a different one.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_agent_folder_refuses_the_launch() {
        use std::os::unix::ffi::OsStrExt;
        let servers = folder_servers();
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff"));
        let context = AgentCtx {
            trusted: true,
            cwd: bad,
            mcp_servers: &servers,
            ..ctx(&[])
        };
        assert_eq!(
            ClaudeAdapter::default()
                .managed(&context, "t", &first(), &none())
                .unwrap_err(),
            LaunchError::McpAgentDirEncoding
        );
    }

    /// At the adapter, the opted-in launch's environment holds each secret under the
    /// name the engine reads, and no argv element holds one. Mutant: put a value in the JSON
    /// instead of a reference.
    #[test]
    fn secrets_reach_the_environment_and_never_the_argv() {
        let servers = declared_servers(&["memory", "notes", "search"]);
        let daemon_env = [EnvVar::new("PATH", "/usr/bin")];
        let context = AgentCtx {
            trusted: true,
            mcp_servers: &servers,
            ..ctx(&daemon_env)
        };
        let spec = ClaudeAdapter::default()
            .managed(&context, "t", &first(), &none())
            .expect("a launch");
        assert_eq!(
            minus_guard_env(spec.env),
            [
                EnvVar::new(
                    "DUDES_MCP_MEMORY_HEADER_0",
                    crate::mcp::fixture::MEMORY_AUTH
                ),
                EnvVar::new("DUDES_MCP_SEARCH_BEARER", crate::mcp::fixture::SEARCH_KEY),
                EnvVar::new("NOTES_API_KEY", crate::mcp::fixture::NOTES_API_KEY),
                EnvVar::new("PATH", "/usr/bin"),
            ]
        );
        for value in [
            crate::mcp::fixture::MEMORY_AUTH,
            crate::mcp::fixture::NOTES_API_KEY,
            crate::mcp::fixture::SEARCH_KEY,
        ] {
            assert!(
                spec.argv
                    .iter()
                    .all(|part| !part.to_string_lossy().contains(value))
            );
        }
        let plain = ClaudeAdapter::default()
            .managed(&ctx(&daemon_env), "t", &first(), &none())
            .expect("a launch");
        assert_eq!(
            minus_guard_env(plain.env),
            [EnvVar::new("PATH", "/usr/bin")]
        );
    }

    /// Program and http servers under the default isolation only.
    #[test]
    fn only_the_default_isolation_loads_declared_servers() {
        for (isolation, support) in [
            (IsolationMode::Default, McpSupport::Program),
            (IsolationMode::Bare, McpSupport::None),
            (IsolationMode::Inherit, McpSupport::None),
        ] {
            let adapter = ClaudeAdapter::new(ClaudeConfig {
                isolation,
                ..ClaudeConfig::default()
            });
            assert_eq!(adapter.mcp_support(), support, "{isolation:?}");
        }
    }

    fn configured_argv(config: ClaudeConfig) -> Vec<OsString> {
        ClaudeAdapter::new(config)
            .managed(&ctx(&[]), "t", &first(), &none())
            .expect("the override is a launch")
            .argv
    }

    fn default_argv_with_tools(tools: &[&str]) -> Vec<OsString> {
        let mut want = os(&HEAD);
        want.extend(isolation(""));
        want.extend(os(&[
            "--session-id",
            UUID,
            "--dangerously-skip-permissions",
        ]));
        want.extend(os(tools));
        want.extend(os(&["--", "t"]));
        want
    }

    #[test]
    fn disallowed_tools_changes_only_its_argv_slice() {
        let config = ClaudeConfig {
            disallowed_tools: vec!["Bash(rm *)".to_owned(), "WebFetch".to_owned()],
            ..ClaudeConfig::default()
        };
        assert_eq!(
            configured_argv(config),
            default_argv_with_tools(&["--disallowedTools", "Bash(rm *)", "WebFetch"])
        );
    }

    #[test]
    fn allowed_tools_changes_only_its_argv_slice() {
        let config = ClaudeConfig {
            allowed_tools: vec!["Read".to_owned(), "Grep".to_owned()],
            ..ClaudeConfig::default()
        };
        assert_eq!(
            configured_argv(config),
            default_argv_with_tools(&["--allowedTools", "Read", "Grep"])
        );
    }

    #[test]
    fn tools_changes_only_its_argv_slice() {
        let config = ClaudeConfig {
            tools: vec!["Read".to_owned(), "Edit".to_owned()],
            ..ClaudeConfig::default()
        };
        assert_eq!(
            configured_argv(config),
            default_argv_with_tools(&["--tools", "Read", "Edit"])
        );
    }

    #[test]
    fn an_untrusted_project_source_override_is_refused() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            setting_sources: Some(vec![SettingSource::User, SettingSource::Project]),
            ..ClaudeConfig::default()
        });
        assert_eq!(
            adapter.managed(&ctx(&[]), "t", &first(), &none()),
            Err(LaunchError::UntrustedRepositorySettings)
        );
        let trusted = adapter
            .managed(&trusted_ctx(&[]), "t", &first(), &none())
            .expect("recorded trust admits the configured project source");
        let source = trusted
            .argv
            .iter()
            .position(|part| part == "--setting-sources")
            .expect("the configured source flag");
        assert_eq!(trusted.argv[source + 1], "user,project");

        let narrowed = ClaudeAdapter::new(ClaudeConfig {
            setting_sources: Some(vec![SettingSource::User, SettingSource::Local]),
            ..ClaudeConfig::default()
        })
        .managed(&ctx(&[]), "t", &first(), &none())
        .expect("an untrusted override may omit project");
        let source = narrowed
            .argv
            .iter()
            .position(|part| part == "--setting-sources")
            .expect("the configured source flag");
        assert_eq!(narrowed.argv[source + 1], "user,local");
    }

    #[test]
    fn hire_refuses_untrusted_inherit() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            ..ClaudeConfig::default()
        });
        assert_eq!(
            adapter.managed(&ctx(&[]), "t", &first(), &none()),
            Err(LaunchError::UntrustedRepositorySettings)
        );
        assert!(
            adapter
                .managed(&trusted_ctx(&[]), "t", &first(), &none())
                .is_ok(),
            "recorded trust is the opposite-polarity control"
        );
    }

    #[test]
    fn hire_refuses_when_suppression_cannot_be_expressed() {
        let adapter = ClaudeAdapter::default();
        assert_eq!(
            adapter.setting_sources(false, false),
            Err(LaunchError::UntrustedRepositorySettings)
        );
        assert_eq!(
            adapter.setting_sources(false, true),
            Ok(Some(Vec::new())),
            "the accepted 2.1.263 capability selects the production branch"
        );
    }

    /// The optional parts land in this order: caps after the session, the permission
    /// flag after the caps, `extra_args` last before `--`; `permission_mode = default` adds
    /// nothing.
    #[test]
    fn caps_permission_and_extra_args_are_appended_in_order_before_the_separator() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: os(&["--model", "haiku"]),
            ..ClaudeConfig::default()
        });
        let mut c = ctx(&[]);
        c.max_turns = Some(3);
        c.max_budget = Some(Usd::from_cents(250));
        let argv = adapter
            .managed(&c, "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        let tail = &argv[HEAD.len() + isolation("").len()..];
        assert_eq!(
            tail,
            os(&[
                "--session-id",
                UUID,
                "--max-turns",
                "3",
                "--max-budget-usd",
                "2.500000",
                "--dangerously-skip-permissions",
                "--model",
                "haiku",
                "--",
                "t",
            ])
        );

        let plain = ClaudeAdapter::new(ClaudeConfig {
            permission_mode: PermissionMode::Default,
            ..ClaudeConfig::default()
        });
        let argv = plain
            .managed(&ctx(&[]), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        assert!(!argv.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert_eq!(&argv[argv.len() - 2..], os(&["--", "t"]));
    }

    /// `--max-budget-usd` carries the padded decimal from [`Usd::to_decimal`], and the
    /// sub-dollar case is the one that matters: five cents is `0.050000`. A ceiling read as
    /// `0.5` would be ten times the operator's, and the flag takes no currency sign.
    #[test]
    fn sub_dollar_budget_reaches_the_flag_fully_padded() {
        let adapter = ClaudeAdapter::new(ClaudeConfig::default());
        let mut c = ctx(&[]);
        c.max_budget = Some(Usd::from_cents(5));
        let argv = adapter
            .managed(&c, "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        let flag = argv
            .iter()
            .position(|a| a == "--max-budget-usd")
            .expect("the flag is present when a ceiling is set");
        assert_eq!(argv[flag + 1], OsString::from("0.050000"));
        assert_eq!(
            argv[flag + 1],
            OsString::from(Usd::from_cents(5).to_decimal())
        );
    }

    /// Invariant 7: the element count is the same for `t` and for a task full of shell
    /// metacharacters (the task stays one element), no fixed flag part carries whitespace,
    /// and an `agent_dir` with a space is one element too — values may contain spaces,
    /// joins may not.
    ///
    /// Both task routes, because the invariant is about the flag parts and those are the same
    /// either way. The two arms differ only in the tail: the argv form ends `-- <task>`, the
    /// stdin form ends at the last flag and carries the identical bytes on `stdin`. The large
    /// task is the small one repeated, so it holds the same metacharacters and the two arms
    /// differ by size alone — which is the only thing that decides the route.
    #[test]
    fn no_argv_element_is_a_space_joined_pair() {
        let task = "echo \"a b\" && rm -rf / | cat $(id) ; --resume x";
        let ceiling = ClaudeConfig::default().task_argv_max_bytes;
        let big = task.repeat(ceiling / task.len() + 1);
        assert!(big.len() > ceiling, "the large arm must cross the ceiling");
        let spaced = Path::new("/tmp/dudes data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let variants = [
            ClaudeConfig::default(),
            ClaudeConfig {
                permission_mode: PermissionMode::Default,
                ..ClaudeConfig::default()
            },
            ClaudeConfig {
                isolation: IsolationMode::Inherit,
                ..ClaudeConfig::default()
            },
            ClaudeConfig {
                extra_args: os(&["--max-thinking-tokens", "100"]),
                ..ClaudeConfig::default()
            },
        ];
        for config in variants {
            let adapter = ClaudeAdapter::new(config);
            for session in [first(), TurnSession::Resume("s".into())] {
                for (task, on_stdin) in [(task, false), (big.as_str(), true)] {
                    let mut base = ctx(&[]);
                    base.trusted = adapter.config().isolation == IsolationMode::Inherit;
                    let plain = adapter
                        .managed(&base, "t", &session, &none())
                        .expect("a non-empty task without NUL is a launch")
                        .argv;
                    let c = AgentCtx {
                        agent_dir: spaced,
                        ..base
                    };
                    let spec = adapter
                        .managed(&c, task, &session, &none())
                        .expect("a non-empty task without NUL is a launch");
                    let argv = &spec.argv;
                    // The flag parts every element below is scanned from: the whole argv in the
                    // stdin form, everything but the task in the argv form.
                    let flags = if on_stdin {
                        assert_eq!(argv.len(), plain.len() - 2, "`--` and the task both go");
                        assert!(!argv.iter().any(|a| a == "--"), "no dangling separator");
                        assert!(!argv.iter().any(|a| a == task), "no task element");
                        assert_eq!(
                            spec.stdin,
                            crate::launch::LaunchInput::WriteThenClose(task.as_bytes().to_vec())
                        );
                        &argv[..]
                    } else {
                        assert_eq!(argv.len(), plain.len());
                        assert_eq!(argv.last().unwrap(), task);
                        assert_eq!(argv[argv.len() - 2], "--");
                        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
                        &argv[..argv.len() - 1]
                    };
                    let value = argv.iter().position(|a| a == "--settings").map(|i| i + 1);
                    for (i, part) in flags.iter().enumerate() {
                        let text = part.to_str().unwrap();
                        if Some(i) == value {
                            assert_eq!(part, &spaced.join(MANAGED_SETTINGS_FILE));
                            continue;
                        }
                        assert!(!text.contains(char::is_whitespace), "{text:?} is joined");
                    }
                }
            }
        }
    }

    /// The surviving clauses of the environment scrub: the child's environment is the daemon's minus
    /// the nesting variables — by exact name and by prefix — and everything else, the vendor
    /// credential included, is passed through untouched with nothing added.
    #[test]
    fn scrubbed_env_carries_no_nesting_variable() {
        let mut daemon: Vec<EnvVar> = NESTING_VARS
            .iter()
            .map(|name| EnvVar::new(*name, "x"))
            .collect();
        daemon.extend(
            [
                ("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/s"),
                ("CLAUDE_CODE_MESSAGING_TOKEN", "t"),
                ("PATH", "/usr/bin"),
                ("ANTHROPIC_API_KEY", "sk-test"),
                ("CLAUDE_CONFIG_DIR", "/home/x/.claude"),
            ]
            .map(|(k, v)| EnvVar::new(k, v)),
        );
        let env = ClaudeAdapter::default()
            .managed(&ctx(&daemon), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .env;
        let env = minus_guard_env(env);
        let mut want = [
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("CLAUDE_CONFIG_DIR", "/home/x/.claude"),
            ("PATH", "/usr/bin"),
        ]
        .map(|(k, v)| EnvVar::new(k, v))
        .to_vec();
        want.sort();
        assert_eq!(env, want);
    }

    /// Windows owns case-insensitive environment names, so a parent nesting marker can arrive
    /// in casing the CLI did not originally set. Exact matching has to remove that marker while
    /// leaving a similarly shaped but distinct name alone. The daemon order is deliberately not
    /// the expected order, so this remains an additional guard for [`scrub_env`]'s final sort.
    #[cfg(windows)]
    #[test]
    fn windows_scrub_matches_mixed_case_exact_nesting_names() {
        let daemon = [
            ("z-survivor", "z"),
            ("cLaUdEcOdE", "nested"),
            ("cLaUdEcOdEs", "not-a-nesting-name"),
            ("A-survivor", "a"),
        ]
        .map(|(name, value)| EnvVar::new(name, value));

        let env = scrub_env(&daemon);
        let mut want = [
            ("z-survivor", "z"),
            ("cLaUdEcOdEs", "not-a-nesting-name"),
            ("A-survivor", "a"),
        ]
        .map(|(name, value)| EnvVar::new(name, value))
        .to_vec();
        want.sort();

        assert_eq!(env, want);
    }

    /// The Windows case-insensitivity applies equally to the prefix arm. The missing underscore in the
    /// positive control is intentional: only descendants of `CLAUDE_CODE_MESSAGING_` are nesting
    /// variables, even when the shared prefix has mixed casing.
    #[cfg(windows)]
    #[test]
    fn windows_scrub_matches_mixed_case_nesting_prefixes() {
        let daemon = [
            ("z-survivor", "z"),
            ("cLaUdE_cOdE_mEsSaGiNg_sOcKeT", "nested"),
            ("cLaUdE_cOdE_mEsSaGiNg", "not-a-nesting-name"),
            ("A-survivor", "a"),
        ]
        .map(|(name, value)| EnvVar::new(name, value));

        let env = scrub_env(&daemon);
        let mut want = [
            ("z-survivor", "z"),
            ("cLaUdE_cOdE_mEsSaGiNg", "not-a-nesting-name"),
            ("A-survivor", "a"),
        ]
        .map(|(name, value)| EnvVar::new(name, value))
        .to_vec();
        want.sort();

        assert_eq!(env, want);
    }

    /// On non-Windows targets a differently cased name is not one of the exact nesting names or
    /// prefixes. This is the counter-control to the Windows tests: a global case fold would make
    /// both values disappear.
    #[cfg(not(windows))]
    #[test]
    fn non_windows_scrub_keeps_mixed_case_nesting_lookalikes() {
        let daemon = [
            ("z-survivor", "z"),
            ("cLaUdEcOdE", "not-a-nesting-name"),
            ("cLaUdE_cOdE_mEsSaGiNg_sOcKeT", "not-a-nesting-name"),
            ("A-survivor", "a"),
        ]
        .map(|(name, value)| EnvVar::new(name, value));

        let env = scrub_env(&daemon);
        let mut want = daemon.to_vec();
        want.sort();

        assert_eq!(env, want);
    }

    /// CLI 2.1.251: `CLAUDE_CODE_SIMPLE` suppresses hooks **even when `--settings`
    /// names the file explicitly**, so a managed child that inherited it would be Tier 1 in
    /// name only. It is therefore stripped, and this asserts that it is.
    ///
    /// It names the variable as a **string literal** and derives nothing from [`NESTING_VARS`],
    /// which is the whole reason it exists beside `scrubbed_env_carries_no_nesting_variable`:
    /// that test builds its daemon environment *from* the constant, so deleting a name removes
    /// it from the fixture too and its assertion still holds. Measured under the mutation
    /// `- "CLAUDE_CODE_SIMPLE",` — that test stays green, this one goes red. The survivor arm
    /// is the in-test positive control: without it the absence assertion would also pass if the
    /// scrub dropped the whole environment.
    #[test]
    fn claude_code_simple_never_reaches_a_managed_child() {
        let daemon = [
            ("CLAUDE_CODE_SIMPLE", "1"),
            ("PATH", "/usr/bin"),
            ("ANTHROPIC_API_KEY", "sk-test"),
        ]
        .map(|(k, v)| EnvVar::new(k, v));
        let env = ClaudeAdapter::default()
            .managed(&ctx(&daemon), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .env;
        let names: Vec<&OsStr> = env.iter().map(EnvVar::name).collect();
        assert!(
            !names.contains(&OsStr::new("CLAUDE_CODE_SIMPLE")),
            "CLAUDE_CODE_SIMPLE reached a managed child: {names:?}"
        );
        for survivor in ["PATH", "ANTHROPIC_API_KEY"] {
            assert!(
                names.contains(&OsStr::new(survivor)),
                "{survivor} must reach the child untouched; without this arm the test \
                 would also pass if the scrub dropped everything"
            );
        }
    }

    /// `DUDES_SOCK` and `DUDES_TOKEN` are set by the supervisor on every child it
    /// launches for an agent, managed and attached alike, so the adapter passes them through —
    /// removing what the supervisor deliberately set would break the supervisor's own token wiring.
    /// The nesting scrub is unchanged, and the adapter still adds neither
    /// pair itself. The two pass-through assertions are deliberately asymmetric, and that
    /// asymmetry is the point. A socket path is not a credential, so `DUDES_SOCK` is compared by
    /// value and a failure prints both paths. A token is, so `DUDES_TOKEN` is compared in two
    /// parts: `assert_eq!` on the name, which prints, and a boolean check on the value, whose
    /// message reports a mismatch without printing either side. Comparing the whole pair with
    /// `assert_eq!` would be worse than useless for the token — [`EnvVar`]'s `Debug` redacts, so
    /// both sides of a real difference would print `"DUDES_TOKEN"=<redacted>` and diagnose
    /// nothing.
    #[test]
    fn managed_env_keeps_dudes_sock_and_token() {
        let sock = EnvVar::new("DUDES_SOCK", "/tmp/dudes-data/run/duded.sock");
        let token = EnvVar::new("DUDES_TOKEN", "fixture-token-not-a-credential");
        let daemon = vec![
            EnvVar::new("CLAUDECODE", "1"),
            EnvVar::new("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/s"),
            sock.clone(),
            token.clone(),
            EnvVar::new("PATH", "/usr/bin"),
        ];
        let env = ClaudeAdapter::default()
            .managed(&ctx(&daemon), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .env;
        let var = |name: &str| env.iter().find(|var| var.name() == OsStr::new(name));
        assert_eq!(
            var("DUDES_SOCK").map(EnvVar::expose_value),
            Some(sock.expose_value()),
            "DUDES_SOCK must reach the managed child unchanged"
        );
        assert_eq!(
            var("DUDES_TOKEN").map(EnvVar::name),
            Some(token.name()),
            "DUDES_TOKEN must reach the managed child"
        );
        assert!(
            var("DUDES_TOKEN").map(EnvVar::expose_value) == Some(token.expose_value()),
            "DUDES_TOKEN reached the managed child with a different value \
             (not printed: it is a credential)"
        );
        assert_eq!(var("CLAUDECODE"), None, "a nesting variable still goes");
        assert_eq!(
            var("CLAUDE_CODE_MESSAGING_SOCKET"),
            None,
            "a prefix-matched nesting variable still goes"
        );
        assert_eq!(minus_guard_env(env).len(), 3, "and nothing is added");
    }

    /// `managed-settings.json` is a projection — equal bytes
    /// from equal inputs, exactly the two keys (plus `model` when pinned) and none of the
    /// keys the isolation settings exclude. The file lands under the agent's directory at the path the
    /// argv's `--settings` names.
    #[test]
    fn managed_settings_is_byte_stable_and_exactly_the_expected_content() {
        let adapter = ClaudeAdapter::default();
        let install = adapter.managed_install(&ctx(&[]), &none());
        assert_eq!(install, adapter.managed_install(&ctx(&[]), &none()));
        assert_eq!(
            minus_guard_files(install.files),
            vec![(
                Path::new(AGENT_DIR).join(MANAGED_SETTINGS_FILE),
                "{\"autoMemoryEnabled\":false,\"disableAllHooks\":true}\n".to_owned()
            )]
        );
        let pinned = ClaudeAdapter::new(ClaudeConfig {
            model: Some(MODEL.to_owned()),
            ..ClaudeConfig::default()
        });
        let text = pinned.managed_settings(&pin(MODEL));
        assert_eq!(
            text,
            "{\"autoMemoryEnabled\":false,\"disableAllHooks\":true,\"model\":\"claude-haiku-4-5\"}\n"
        );
        let parsed: Value = serde_json::from_str(&text).unwrap();
        for key in [
            "hooks",
            "permissions",
            "enabledPlugins",
            "env",
            "apiKeyHelper",
        ] {
            assert!(parsed.get(key).is_none(), "{key} must not be written");
        }

        let inherit = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            ..ClaudeConfig::default()
        });
        assert_eq!(inherit.managed_settings(&none()), "{}\n");
        assert_eq!(
            inherit.managed_settings(&pin(MODEL)),
            "{\"model\":\"claude-haiku-4-5\"}\n"
        );
    }

    /// `isolation = inherit` drops exactly the isolation set and its file, nothing else, and
    /// the adapter then declares `Isolation::None` so the hire log can say so.
    #[test]
    fn inherit_drops_the_isolation_set_and_nothing_else() {
        let isolated = ClaudeAdapter::default();
        let inherit = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            ..ClaudeConfig::default()
        });
        let mut want = isolated
            .managed(&ctx(&[]), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        want.drain(HEAD.len()..HEAD.len() + isolation("").len());
        assert_eq!(
            inherit
                .managed(&trusted_ctx(&[]), "t", &first(), &none())
                .expect("a non-empty task without NUL is a launch")
                .argv,
            want
        );
        assert_eq!(
            minus_guard_files(inherit.managed_install(&trusted_ctx(&[]), &none()).files),
            vec![]
        );
        assert_eq!(isolated.isolation(), Isolation::Flags(ISOLATION_FLAGS));
        assert_eq!(inherit.isolation(), Isolation::None);

        // A pinned model survives inherit: `--settings` alone, a file carrying only `model`.
        let pinned = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            model: Some(MODEL.to_owned()),
            ..ClaudeConfig::default()
        });
        let mut with_model = want.clone();
        with_model.splice(
            HEAD.len()..HEAD.len(),
            [OsString::from("--settings"), settings()],
        );
        assert_eq!(
            pinned
                .managed(&trusted_ctx(&[]), "t", &first(), &pin(MODEL))
                .expect("a non-empty task without NUL is a launch")
                .argv,
            with_model
        );
        assert_eq!(
            minus_guard_files(pinned.managed_install(&trusted_ctx(&[]), &pin(MODEL)).files),
            vec![(
                Path::new(AGENT_DIR).join(MANAGED_SETTINGS_FILE),
                "{\"model\":\"claude-haiku-4-5\"}\n".to_owned()
            )]
        );
    }

    /// `isolation = "bare"` puts `--bare` where the isolation set goes and
    /// composes nothing else — no `--setting-sources`, no `--strict-mcp-config` and no
    /// settings file, unless a model is pinned: `--settings` is explicit rather than
    /// discovered, so bare keeps it for the same reason `inherit` does. Byte for byte, then
    /// the negative — none of the default set survives.
    #[test]
    fn bare_replaces_the_isolation_set_with_the_bare_flag() {
        let bare = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Bare,
            ..ClaudeConfig::default()
        });
        let mut want = os(&HEAD);
        want.extend(os(&[
            "--bare",
            "--session-id",
            UUID,
            "--dangerously-skip-permissions",
            "--",
            "t",
        ]));
        let argv = bare
            .managed(&ctx(&[]), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        assert_eq!(argv, want);
        for part in ISOLATION_FLAGS {
            assert!(
                !argv.iter().any(|a| a == part),
                "`{part}` must not survive bare"
            );
        }
        assert_eq!(
            minus_guard_files(bare.managed_install(&ctx(&[]), &none()).files),
            vec![]
        );
        assert_eq!(bare.isolation(), Isolation::Flags(BARE_FLAG));

        // A pinned model survives, referenced by `--settings` alone.
        let pinned = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Bare,
            model: Some(MODEL.to_owned()),
            ..ClaudeConfig::default()
        });
        let mut with_model = want.clone();
        let after_bare = HEAD.len() + 1;
        with_model.splice(
            after_bare..after_bare,
            [OsString::from("--settings"), settings()],
        );
        assert_eq!(
            pinned
                .managed(&ctx(&[]), "t", &first(), &pin(MODEL))
                .expect("a non-empty task without NUL is a launch")
                .argv,
            with_model
        );
        assert_eq!(
            minus_guard_files(pinned.managed_install(&ctx(&[]), &pin(MODEL)).files),
            vec![(
                Path::new(AGENT_DIR).join(MANAGED_SETTINGS_FILE),
                "{\"model\":\"claude-haiku-4-5\"}\n".to_owned()
            )]
        );
    }

    /// a launch is composed from **the selection it is handed**, never from the
    /// adapter's own configured model. Every other test in this module hands the adapter a
    /// selection built from [`MODEL`] — the same constant its configuration names — so the two are
    /// indistinguishable there, and a site that read `self.config.model` back would pass all of
    /// them. Configuration and selection differ at exactly one moment in production, a re-issue
    /// after a configuration change, and this is that moment written down.
    ///
    /// **Arm one: what the settings file pins.** Configuration says `model-beta`, the turn's
    /// recorded selection says `model-alpha`, and the file the launch installs pins alpha — the
    /// journal and the launch material describe one turn, so they may not name two models.
    ///
    /// **Arm two: whether a settings file is referenced at all.** Under `inherit` that file exists
    /// only because a model is pinned, and here the configuration pins one while the turn
    /// deliberately took the provider's default. Both gates that ask "is a model pinned?" — the
    /// `--settings` decision in [`ClaudeAdapter::argv`] and the `referenced` decision in
    /// [`Adapter::managed_install`] — must ask it of the selection, so the argv carries no
    /// `--settings` and the install carries no file.
    ///
    /// Each arm carries its own control, because both are negatives about a value the subject
    /// could also produce for itself.
    #[test]
    fn a_launch_is_composed_from_the_given_selection_not_the_configured_one() {
        const ALPHA: &str = "model-alpha";
        const BETA: &str = "model-beta";
        let configured = ClaudeAdapter::new(ClaudeConfig {
            model: Some(BETA.to_owned()),
            ..ClaudeConfig::default()
        });
        let recorded = pin(ALPHA);
        assert_eq!(
            configured.model_selection(),
            pin(BETA),
            "control: the configured model is beta, so an arm below that reported beta would be \
             reporting the configuration rather than the selection"
        );
        assert_eq!(
            configured.managed_settings(&recorded),
            "{\"autoMemoryEnabled\":false,\"disableAllHooks\":true,\"model\":\"model-alpha\"}\n",
            "the settings file pins the recorded selection"
        );
        assert_eq!(
            minus_guard_files(configured.managed_install(&ctx(&[]), &recorded).files),
            vec![(
                Path::new(AGENT_DIR).join(MANAGED_SETTINGS_FILE),
                "{\"autoMemoryEnabled\":false,\"disableAllHooks\":true,\"model\":\"model-alpha\"}\n"
                    .to_owned()
            )],
            "and so does the file the install writes"
        );

        // Arm two: a configured model, a selection that took the provider's default, and the one
        // isolation whose settings file exists for a pinned model and for nothing else.
        let inherit = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            model: Some(BETA.to_owned()),
            ..ClaudeConfig::default()
        });
        assert_eq!(
            minus_guard_files(inherit.managed_install(&ctx(&[]), &none()).files),
            vec![],
            "no model was selected, so there is no file to install"
        );
        let argv = inherit
            .managed(&trusted_ctx(&[]), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch")
            .argv;
        assert!(
            !argv.iter().any(|a| a == "--settings"),
            "no model was selected, so nothing references a settings file: {argv:?}"
        );
        // Control on both negatives: the same adapter, handed a selection that names a model,
        // does compose the flag and does write the file — so the two assertions above are not
        // passing because `inherit` never references a settings file.
        let selected = inherit
            .managed(&trusted_ctx(&[]), "t", &first(), &pin(ALPHA))
            .expect("a non-empty task without NUL is a launch");
        assert!(
            selected.argv.iter().any(|a| a == "--settings"),
            "control: a selected model still composes `--settings`"
        );
        assert_eq!(
            minus_guard_files(inherit.managed_install(&ctx(&[]), &pin(ALPHA)).files),
            vec![(
                Path::new(AGENT_DIR).join(MANAGED_SETTINGS_FILE),
                "{\"model\":\"model-alpha\"}\n".to_owned()
            )],
            "control: a selected model still installs the file"
        );
    }

    /// An agent directory whose *first byte is a dash*, so the `--settings` value the adapter
    /// joins from it is itself dash-leading. Relative rather than absolute for exactly that
    /// reason: an absolute path begins with a separator and could never exercise the case.
    const DASH_AGENT_DIR: &str = "-dash-agent-dir";

    /// A session id whose first byte is a dash. The adapter wraps vendor text without
    /// validating it, so this reaches the argv as `--session-id -dash-session`.
    const DASH_SESSION: &str = "-dash-session";

    /// A task whose first bytes are dashes, so the walk is proved to stop at `--`.
    const DASH_TASK: &str = "--dash-task-with-leading-dashes";

    /// How many argv elements follow a flag as its values.
    ///
    /// `Unknown` is not a state of ignorance to be tidied away: it is the arm that keeps
    /// [`composed_flags`] safe against a flag nobody has classified yet, and its value count is
    /// **zero** on purpose. See that function.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Arity {
        Fixed(usize),
        Variadic,
        Unknown,
    }

    /// The declared arity of every flag [`ClaudeAdapter::argv`] composes today.
    ///
    /// This table is *not* the thing under test — `RESERVED_EXTRA_ARGS` is. It exists so the
    /// walker can skip a flag's **values** without inspecting them, which is what lets a
    /// dash-leading value (a settings path, an MCP path, a session id) travel through the
    /// matrix without being mistaken for a flag.
    fn arity(flag: &str) -> Arity {
        match flag {
            "-p"
            | "--verbose"
            | "--strict-mcp-config"
            | "--bare"
            | "--dangerously-skip-permissions" => Arity::Fixed(0),
            "--output-format" | "--setting-sources" | "--settings" | "--session-id"
            | "--resume" | "--mcp-config" | "--max-turns" | "--max-budget-usd" => Arity::Fixed(1),
            "--disallowedTools" | "--allowedTools" | "--tools" => Arity::Variadic,
            _ => Arity::Unknown,
        }
    }

    /// Every flag-shaped element of one composed argv, by name, with `<flag>=<value>` normalised
    /// to the flag.
    ///
    /// Three rules, each load-bearing rather than a parsing convenience:
    ///
    /// - **A fixed-arity flag's values are skipped by count, never inspected.** That is what
    ///   makes a dash-leading *value* — `--settings -dash-agent-dir/managed-settings.json` —
    ///   a value here and not a second flag.
    /// - **A variadic flag's values run until the next dash-leading token**, not until the next
    ///   *known* flag. [`append_list`] emits the three tool flags immediately before
    ///   `extra_args`, so a flag pushed inline after them is precisely the token a
    ///   stop-at-a-known-flag walker swallows. The cost is that a dash-leading tool rule would
    ///   be reported as a flag and redden this test; that is a false red, and the right
    ///   direction. **Do not add an exemption list for dash-leading tool rules** — it converts
    ///   the false red into a silent green, which is the failure this test exists to stop.
    /// - **An unknown flag consumes no values at all.** Its value, if dash-leading, is then
    ///   reported as a second unknown flag: conservative, and red either way. Guessing an arity
    ///   of one would let a newly composed value-taking flag hide the token after it.
    ///
    /// The walk records `--` and stops there: everything after the separator is the task, and a
    /// task may begin with dashes.
    fn composed_flags(argv: &[OsString]) -> Vec<String> {
        let mut found = Vec::new();
        let mut index = 1; // argv[0] is the program, never a flag.
        while index < argv.len() {
            let element = argv[index].to_string_lossy().into_owned();
            if element == "--" {
                found.push("--".to_owned());
                break;
            }
            if !element.starts_with('-') {
                index += 1;
                continue;
            }
            let (name, carries_value) = match element.split_once('=') {
                Some((head, _)) => (head.to_owned(), true),
                None => (element, false),
            };
            index += 1;
            index += match arity(&name) {
                Arity::Fixed(values) if !carries_value => values,
                // A `<flag>=<value>` element already carries its own value, and an unclassified
                // flag has no value count anyone can trust.
                Arity::Fixed(_) | Arity::Unknown => 0,
                Arity::Variadic => {
                    let mut values = 0;
                    while index + values < argv.len()
                        && !argv[index + values].to_string_lossy().starts_with('-')
                    {
                        values += 1;
                    }
                    values
                }
            };
            found.push(name);
        }
        found
    }

    /// One configuration of the matrix. `lists` drives the three variadic tool flags and the one
    /// `--mcp-config` together, because the row that matters most is the one where **every**
    /// variadic span is empty.
    fn leading_dash_config(
        isolation: IsolationMode,
        permission: PermissionMode,
        lists: bool,
    ) -> ClaudeConfig {
        ClaudeConfig {
            isolation,
            permission_mode: permission,
            model: Some(MODEL.to_owned()),
            setting_sources: Some(vec![SettingSource::Project]),
            disallowed_tools: if lists {
                vec!["Bash".to_owned(), "Write".to_owned()]
            } else {
                Vec::new()
            },
            allowed_tools: if lists {
                vec!["Read".to_owned()]
            } else {
                Vec::new()
            },
            tools: if lists {
                vec!["Edit".to_owned()]
            } else {
                Vec::new()
            },
            ..ClaudeConfig::default()
        }
    }

    /// One row's argv, composed by running the adapter rather than by describing it.
    fn leading_dash_row(
        isolation: IsolationMode,
        permission: PermissionMode,
        lists: bool,
        caps: bool,
        session: &TurnSession,
        selection: &ModelSelection,
        on_stdin: bool,
    ) -> Vec<OsString> {
        let env = [EnvVar::new(
            "ANTHROPIC_API_KEY",
            "fixture-value-not-a-credential",
        )];
        let config = leading_dash_config(isolation, permission, lists);
        let ceiling = config.task_argv_max_bytes;
        let adapter = ClaudeAdapter::new(config);
        let servers = declared_servers(&["memory"]);
        let mut context = trusted_ctx(&env);
        context.agent_dir = Path::new(DASH_AGENT_DIR);
        if lists {
            context.mcp_servers = &servers;
        }
        if caps {
            context.max_turns = Some(7);
            context.max_budget = Some(Usd::from_micros(1_500_000));
        }
        let task = if on_stdin {
            "t".repeat(ceiling + 1)
        } else {
            DASH_TASK.to_owned()
        };
        adapter
            .managed(&context, &task, session, selection)
            .expect("every row of the matrix is a launch")
            .argv
    }

    /// Every argv the adapter composes across the branches of [`ClaudeAdapter::argv`], each
    /// labelled with the row that produced it.
    fn leading_dash_matrix() -> Vec<(String, Vec<OsString>)> {
        let mut rows = Vec::new();
        for (iname, isolation) in [
            ("default", IsolationMode::Default),
            ("bare", IsolationMode::Bare),
            ("inherit", IsolationMode::Inherit),
        ] {
            for (sname, session) in [
                ("first", TurnSession::First(SessionId::from(DASH_SESSION))),
                ("resume", TurnSession::Resume(SessionId::from(DASH_SESSION))),
            ] {
                for (pname, permission) in [
                    ("bypass", PermissionMode::Bypass),
                    ("ask", PermissionMode::Default),
                ] {
                    for (mname, selection) in [("pinned", pin(MODEL)), ("nomodel", none())] {
                        for (cname, caps) in [("caps", true), ("nocaps", false)] {
                            for (lname, lists) in [("lists", true), ("nolists", false)] {
                                for (tname, on_stdin) in [("argv", false), ("stdin", true)] {
                                    rows.push((
                                        format!(
                                            "{iname}/{sname}/{pname}/{mname}/{cname}/{lname}/{tname}"
                                        ),
                                        leading_dash_row(
                                            isolation, permission, lists, caps, &session,
                                            &selection, on_stdin,
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        rows
    }

    /// Every flag [`ClaudeAdapter::argv`] actually composes is one
    /// [`ClaudeConfig::check`] refuses in `extra_args` — derived from the argv the adapter
    /// *returns*, not from the constants it reads.
    ///
    /// That distinction is the point. A test that only walks [`ISOLATION_FLAGS`] and
    /// [`BARE_FLAG`] (as `every_isolation_flag_constant_is_reserved_in_extra_args` does) stays
    /// green with a `--fork-session` pushed inline in `argv`; this test reddens on the `default/first/bypass/pinned/caps/lists/argv` row.
    ///
    /// Four arms, and the order is deliberate:
    ///
    /// 1. **Reservation** — every extracted name is in `RESERVED_EXTRA_ARGS`.
    /// 2. **Coverage** — all seventeen composed spellings were actually seen. A walker that
    ///    returned nothing would sail through arm 1; measured, this is also the arm that catches
    ///    a variadic rule blunted to stop at a known flag, because such a walker eats the `--`.
    /// 3. **Value controls** — three dash-leading *values* that must not have been reported as
    ///    flags. The settings one is built with [`Path::join`], not a `/` literal: on Windows
    ///    the composed element uses a backslash, and a hard-coded `/` would make this assertion
    ///    vacuously true on a platform whose first execution of this code is CI.
    /// 4. **Firing control** — an unreserved flag planted in a real argv is reported, so arm 1's
    ///    silence is the absence of a violation rather than a blind oracle.
    ///
    /// What it still does not guard is named where the list itself is: the four leaks in
    /// `RESERVED_EXTRA_ARGS`'s own doc, which this test cites rather than restates.
    #[test]
    fn every_flag_the_adapter_composes_is_reserved_in_extra_args() {
        use crate::claude::config::RESERVED_EXTRA_ARGS;

        let rows = leading_dash_matrix();
        assert!(
            rows.len() >= 192,
            "the matrix collapsed to {} rows; every branch of argv() must be composed",
            rows.len()
        );

        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (label, argv) in &rows {
            for flag in composed_flags(argv) {
                assert!(
                    RESERVED_EXTRA_ARGS.contains(&flag.as_str()),
                    "`{flag}` is composed by the adapter ({label}) but is not reserved in \
                     extra_args; add it to RESERVED_EXTRA_ARGS"
                );
                seen.insert(flag);
            }
        }

        for composed in [
            "-p",
            "--output-format",
            "--verbose",
            "--include-partial-messages",
            "--setting-sources",
            "--strict-mcp-config",
            "--settings",
            "--bare",
            "--session-id",
            "--resume",
            "--mcp-config",
            "--max-turns",
            "--max-budget-usd",
            "--dangerously-skip-permissions",
            "--disallowedTools",
            "--allowedTools",
            "--tools",
            "--",
        ] {
            assert!(
                seen.contains(composed),
                "the matrix stopped composing {composed}, so nothing above was checked for it"
            );
        }

        let dash_settings = Path::new(DASH_AGENT_DIR)
            .join(MANAGED_SETTINGS_FILE)
            .to_string_lossy()
            .into_owned();
        for value in [DASH_SESSION.to_owned(), dash_settings, DASH_TASK.to_owned()] {
            assert!(
                !seen.contains(&value),
                "`{value}` is a dash-leading value, and the walker reported it as a flag"
            );
        }

        let (_, argv) = rows
            .iter()
            .find(|(label, _)| label.ends_with("/argv"))
            .expect("the matrix composes an argv-task row");
        let mut planted = argv.clone();
        let separator = planted
            .iter()
            .position(|part| part == "--")
            .expect("an argv-task row ends with a separator and a task");
        planted.insert(separator, OsString::from("--dash-unreserved-control"));
        assert!(
            composed_flags(&planted).contains(&"--dash-unreserved-control".to_owned()),
            "the walker must report a flag inserted inline, or arm 1 proves nothing"
        );
    }

    #[test]
    fn the_task_crosses_to_stdin_one_byte_past_the_ceiling() {
        let adapter = ClaudeAdapter::default();
        let max = adapter.config().task_argv_max_bytes;
        let over = "t".repeat(max + 1);
        let mut at = over.clone();
        at.truncate(max);

        let argv_form = adapter
            .managed(&ctx(&[]), &at, &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_eq!(at.len(), max);
        assert_eq!(
            argv_form.stdin,
            crate::launch::LaunchInput::Null,
            "a task at the ceiling is still argv"
        );
        assert_eq!(argv_form.argv.last().unwrap(), &OsString::from(&at));
        assert_eq!(argv_form.argv[argv_form.argv.len() - 2], "--");

        let stdin_form = adapter
            .managed(&ctx(&[]), &over, &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_eq!(over.len(), max + 1);
        assert_eq!(
            stdin_form.stdin,
            crate::launch::LaunchInput::WriteThenClose(over.as_bytes().to_vec()),
            "one byte past the ceiling the task travels on stdin, exactly as given"
        );
        assert_eq!(
            stdin_form.argv,
            argv_form.argv[..argv_form.argv.len() - 2],
            "the argv is the argv form minus `--` and the task, and nothing else moved"
        );
        assert!(!stdin_form.argv.iter().any(|part| part == "--"));
        assert!(!stdin_form.argv.iter().any(|part| part == over.as_str()));

        // And the rest of the spec does not depend on the route.
        assert_eq!(stdin_form.cwd, argv_form.cwd);
        assert_eq!(stdin_form.env, argv_form.env);
    }

    /// The boundary is the operator's number, not a constant: with the ceiling lowered to four
    /// bytes, four bytes is argv and five is stdin — the same pair as above, four orders of
    /// magnitude down, which is what says the test above is reading the config rather than a
    /// hard-coded 65536.
    ///
    /// It is also where the measure is pinned as bytes rather than characters: `"αβγ"` is
    /// three characters and six bytes, so it crosses a five-byte ceiling. A `chars().count()`
    /// implementation passes every other assertion in this file and fails this one — an argv
    /// element is bytes on the way to `execve`, and a character count would hand the kernel
    /// roughly three times what it was promised for text that is not ASCII.
    #[test]
    fn the_ceiling_is_the_configured_number_of_bytes() {
        let low = ClaudeAdapter::new(ClaudeConfig {
            task_argv_max_bytes: 4,
            ..ClaudeConfig::default()
        });
        let at = low
            .managed(&ctx(&[]), "abcd", &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_eq!(at.stdin, crate::launch::LaunchInput::Null);
        assert_eq!(at.argv.last().unwrap(), "abcd");
        assert_eq!(at.argv[at.argv.len() - 2], "--");
        assert_eq!(
            low.managed(&ctx(&[]), "abcde", &first(), &none())
                .expect("a non-empty task without NUL is a launch")
                .stdin,
            crate::launch::LaunchInput::WriteThenClose(b"abcde".to_vec())
        );

        let five = ClaudeAdapter::new(ClaudeConfig {
            task_argv_max_bytes: 5,
            ..ClaudeConfig::default()
        });
        let greek = "αβγ";
        assert_eq!(greek.chars().count(), 3, "three characters");
        assert_eq!(greek.len(), 6, "six bytes");
        assert_eq!(
            five.managed(&ctx(&[]), greek, &first(), &none())
                .expect("a non-empty task without NUL is a launch")
                .stdin,
            crate::launch::LaunchInput::WriteThenClose(greek.as_bytes().to_vec()),
            "six bytes is past a five-byte ceiling however few characters they spell"
        );
        // The control for that one: five ASCII bytes, same ceiling, stays in the argv.
        assert_eq!(
            five.managed(&ctx(&[]), "abcde", &first(), &none())
                .expect("a non-empty task without NUL is a launch")
                .stdin,
            crate::launch::LaunchInput::Null
        );

        // And the default is the documented one, so the number is not only in prose.
        assert_eq!(ClaudeConfig::default().task_argv_max_bytes, 65536);
    }

    /// Exactly what the `Option` in [`ClaudeAdapter::argv`] buys, and where it stops. It ties
    /// that function's own separator to its own element; it does not validate `extra_args`,
    /// which is appended immediately before. So an operator whose `extra_args` ends in a bare
    /// `--` composes a dangling separator in the stdin form — the argv ends in a `--` that no
    /// task follows.
    ///
    /// Recorded rather than endorsed, so the shape is not re-argued from the doc comment.
    /// Refusing `--` in `extra_args` is a change to the reserved-flag check, whose
    /// list is the four flags the adapter composes and does not include `--`; it is not this
    /// function's to make.
    #[test]
    fn an_extra_args_separator_survives_into_the_stdin_form() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: os(&["--"]),
            ..ClaudeConfig::default()
        });
        let over = "t".repeat(adapter.config().task_argv_max_bytes + 1);

        let spec = adapter
            .managed(&ctx(&[]), &over, &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_eq!(
            spec.stdin,
            crate::launch::LaunchInput::WriteThenClose(over.as_bytes().to_vec())
        );
        assert_eq!(
            spec.argv.last().unwrap(),
            "--",
            "the stdin form ends in a separator with nothing after it, composed by extra_args"
        );

        // Two controls, because the claim is about *who* composed it. Same adapter, a task
        // under the ceiling: the separator is not dangling, the task follows it.
        let argv_form = adapter
            .managed(&ctx(&[]), "t", &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_eq!(argv_form.argv.last().unwrap(), "t");
        assert_eq!(argv_form.argv[argv_form.argv.len() - 2], "--");

        // And the same oversized task with no `extra_args` ends at a flag, so it is the
        // operator's element that produced the dangling one and not this function.
        let plain = ClaudeAdapter::default()
            .managed(&ctx(&[]), &over, &first(), &none())
            .expect("a non-empty task without NUL is a launch");
        assert_ne!(
            plain.argv.last().unwrap(),
            "--",
            "without extra_args the stdin form ends at the last flag"
        );
    }
}
