//! Codex managed launches. The default transport is `codex exec --json`; opt-in streaming
//! uses one app-server process with the immutable task in [`super::startup::CodexStartup`].
//!
//! Legacy exec inherits the daemon's `CODEX_HOME` and environment, uses `--ignore-user-config`
//! and `--ignore-rules`, turns off plugins, the apps server and shell snapshots, and selects
//! `workspace-write`.
//! Streaming uses a separate persistent home admitted by the daemon's profile guard, with
//! explicit approval/sandbox settings and a private temporary directory. The daemon resolves
//! paths and excludes project configuration before starting the process and rechecks admission
//! before delivering its sole task frame.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use dudes_types::{ModelSelection, Provider, Tier};
use serde::Serialize;

use crate::adapter::{
    Adapter, AgentCtx, Arrival, Isolation, ManagedInstall, SessionAcquisition, TurnSession,
    UsageSource,
};
use crate::codex::config::CodexConfig;
use crate::codex::stream::TurnStream;
use crate::launch::{LaunchError, LaunchSpec};
use crate::mcp::{self, HttpAuth, McpLaunch, McpRefusal, McpServer, McpSupport};
use crate::stream::TurnReader;

/// Codex assigns the thread id, and `thread.started`, the first line of a run, names it.
pub const SESSION_ACQUISITION: SessionAcquisition = SessionAcquisition::Assigned {
    resume: "resume",
    arrival: Arrival::Start,
};

// `--ignore-user-config` keeps out configuration, not features: a ChatGPT login in `CODEX_HOME`
// still starts Codex's apps server, and exec still syncs plugins. Features go off through `-c`,
// because a Codex that lacks a feature ignores it there but refuses it in `--disable`. A shell
// snapshot is a script under `CODEX_HOME` that exports the launch environment, `DUDES_TOKEN`
// included, and a killed turn leaves it behind.
const ISOLATION: &[&str] = &[
    "--ignore-user-config",
    "--ignore-rules",
    "-c",
    "features.plugins=false",
    "-c",
    "features.apps=false",
    "-c",
    "features.shell_snapshot=false",
];
const SANDBOX: &[&str] = &["-c", "sandbox_mode=\"workspace-write\""];

/// Environment variables that select a Codex billing credential when present and non-empty.
///
/// The vendor's own refusal names them: codex-cli 0.158.0-alpha.2.1's login module reports
/// `auth.json OPENAI_API_KEY CODEX_API_KEY CODEX_ACCESS_TOKEN, no Codex credentials were found`
/// (the binary's strings, 2026-09-27). `auth.json` is a login stored in `CODEX_HOME`, not a
/// variable, so it is not here, as a Claude `/login` is not in that adapter's list.
/// `OPENAI_FEDERATION_RULE_ID` and `OPENAI_IDENTITY_TOKEN_FILE` appear beside them and select a
/// credential the environment does not carry, so they are not members either.
pub(crate) const BILLING_ENV_VARS: &[&str] =
    &["CODEX_API_KEY", "OPENAI_API_KEY", "CODEX_ACCESS_TOKEN"];

/// One declared server as Codex's `mcp_servers.<name>` table.
///
/// `default_tools_approval_mode = "approve"` is on every declared server because Codex runs a
/// managed turn under approval policy `never`, which refuses every tool a server does not mark
/// read-only. Opting an agent in to a server is the operator's approval of all its tools.
#[derive(Serialize)]
struct CodexServer<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer_token_env_var: Option<String>,
    /// Header name to the variable Codex reads its value from.
    #[serde(rename = "env_http_headers", skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<&'a str, String>>,
    default_tools_approval_mode: &'static str,
}

/// One `-c mcp_servers.<name>=<inline table>` pair per server, sorted by name, the table written
/// by the TOML serializer so an operator's URL is one quoted string whatever it holds.
///
/// # Errors
///
/// [`McpRefusal::ProgramUnsupported`] for a program server: Codex starts one in a process group
/// of its own, out of reach of the turn's kill.
fn mcp_options(servers: &[McpLaunch]) -> Result<Vec<OsString>, LaunchError> {
    let mut options = Vec::with_capacity(servers.len() * 2);
    for launch in servers {
        let McpServer::Http(http) = &launch.server else {
            return Err(LaunchError::McpServer(McpRefusal::ProgramUnsupported(
                launch.name.clone(),
            )));
        };
        let (bearer_token_env_var, headers) = match &http.auth {
            HttpAuth::None => (None, None),
            HttpAuth::Bearer(_) => (Some(mcp::bearer_alias(&launch.name)), None),
            HttpAuth::Headers(headers) => (
                None,
                Some(
                    headers
                        .keys()
                        .enumerate()
                        .map(|(position, header)| {
                            (header.as_str(), mcp::header_alias(&launch.name, position))
                        })
                        .collect(),
                ),
            ),
        };
        let mut table = String::new();
        CodexServer {
            url: &http.url,
            bearer_token_env_var,
            headers,
            default_tools_approval_mode: "approve",
        }
        .serialize(toml::ser::ValueSerializer::new(&mut table))
        .expect("a table of strings serialises");
        options.extend([
            OsString::from("-c"),
            OsString::from(format!("mcp_servers.{}={table}", launch.name)),
        ]);
    }
    Ok(options)
}

/// The Codex adapter: pure, like every adapter.
#[derive(Clone, Debug, Default)]
pub struct CodexAdapter {
    config: CodexConfig,
    home: Option<PathBuf>,
}

impl CodexAdapter {
    /// An adapter for the `[providers.codex]` table.
    #[must_use]
    pub const fn new(config: CodexConfig) -> Self {
        Self { config, home: None }
    }
    /// Bind opt-in streaming to the running daemon's persistent profile.
    #[must_use]
    pub fn with_managed_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }
}

impl Adapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn program(&self) -> &OsStr {
        OsStr::new(&self.config.command)
    }

    /// No attached mode, so no hook governance either.
    fn tier(&self) -> Tier {
        Tier::ObservedOnly
    }

    fn isolation(&self) -> Isolation {
        if self.config.streaming {
            Isolation::ConfigDir
        } else {
            Isolation::Flags(ISOLATION)
        }
    }

    fn instruction_manifest(
        &self,
        ctx: &AgentCtx<'_>,
        mode: dudes_types::Mode,
        _selection: &ModelSelection,
    ) -> crate::instructions::InstructionDeclaration {
        use crate::instructions::{DeclarationMode, InstructionDeclaration, codex_manifest};

        if mode == dudes_types::Mode::Attached {
            return InstructionDeclaration::missing(Provider::Codex, mode);
        }
        codex_manifest(
            ctx,
            if self.config.streaming {
                DeclarationMode::CodexStreaming
            } else {
                DeclarationMode::CodexLegacy
            },
            self.home.as_deref(),
        )
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

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
        let declared = mcp_options(ctx.mcp_servers)?;
        let env = mcp::with_secrets(self.child_env(ctx.daemon_env), ctx.mcp_servers);
        if self.config.streaming {
            let home = self
                .home
                .clone()
                .ok_or(LaunchError::ManagedProfileUnavailable)?;
            if matches!(session, TurnSession::First(_)) {
                return Err(LaunchError::SessionModeMismatch);
            }
            let startup = super::startup::CodexStartup::new(
                home,
                ctx.agent_dir.to_owned(),
                ctx.cwd.to_owned(),
                task.to_owned(),
                session.clone(),
                selection.clone(),
            );
            let argv = [
                self.config.command.as_str(),
                "app-server",
                "--listen",
                "stdio://",
                "--strict-config",
                "-c",
                "features.hooks=false",
                "-c",
                "features.plugins=false",
                "-c",
                "features.apps=false",
                "-c",
                "features.shell_snapshot=false",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "sandbox_workspace_write.exclude_tmpdir_env_var=true",
                "-c",
                "sandbox_workspace_write.exclude_slash_tmp=true",
                "-c",
                "sandbox_workspace_write.network_access=false",
            ]
            .into_iter()
            .map(OsString::from)
            .chain(declared)
            .collect();
            return Ok(LaunchSpec {
                argv,
                env,
                cwd: ctx.cwd.to_owned(),
                stdin: crate::launch::LaunchInput::Protocol(startup),
            });
        }
        let mut argv = vec![OsString::from(&self.config.command), OsString::from("exec")];
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        argv.extend([
            OsString::from("--add-dir"),
            ctx.agent_dir.join("outbox").into_os_string(),
        ]);
        argv.extend(SESSION_ACQUISITION.argv(session)?);
        argv.push("--json".into());
        argv.extend(ISOLATION.iter().map(OsString::from));
        argv.push("--skip-git-repo-check".into());
        argv.extend(SANDBOX.iter().map(OsString::from));
        argv.extend(declared);
        if let Some(model) = selection.selector() {
            argv.extend(["-m", model].map(OsString::from));
        }
        argv.extend(["--", task].map(OsString::from));
        Ok(LaunchSpec {
            argv,
            env,
            cwd: ctx.cwd.to_path_buf(),
            stdin: crate::launch::LaunchInput::Null,
        })
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn turn_reader_for(&self, spec: &LaunchSpec) -> Box<dyn TurnReader> {
        match &spec.stdin {
            crate::launch::LaunchInput::Protocol(startup) => {
                Box::new(super::app_server::AppServerReader::new(startup.clone()))
            }
            _ => self.turn_reader(),
        }
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        Some(SANDBOX)
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        BILLING_ENV_VARS
    }

    fn sign_in_check(&self) -> Option<&'static [&'static str]> {
        Some(SIGN_IN_STATUS)
    }

    /// http servers only: Codex starts a program server as the leader of a process group of its
    /// own, which survives a kill of the turn's group.
    fn mcp_support(&self) -> McpSupport {
        McpSupport::Http
    }
}

/// `codex login status`, which exits 0 when signed in and 1 when it finds no login
/// (codex-cli 0.158.0-alpha.2.1).
pub const SIGN_IN_STATUS: &[&str] = &["login", "status"];

/// `codex login`, the vendor's own interactive sign-in.
pub const LOGIN: &[&str] = &["login"];

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dudes_types::{AgentId, SessionId};

    use super::*;
    use crate::launch::EnvVar;

    const TASK: &str = "make the tests pass";
    const THREAD: &str = "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f";

    fn ctx(env: &[EnvVar]) -> AgentCtx<'_> {
        AgentCtx {
            agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid"),
            cwd: Path::new("/repo"),
            agent_dir: Path::new("/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            daemon_env: env,
            sandbox: None,
            trusted: false,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        }
    }

    fn launch(session: &TurnSession, env: &[EnvVar]) -> Result<LaunchSpec, LaunchError> {
        CodexAdapter::default().managed(
            &ctx(env),
            TASK,
            session,
            &ModelSelection::provider_default(),
        )
    }

    fn argv(parts: &[&str]) -> Vec<OsString> {
        let expected: Vec<_> = parts.iter().map(OsString::from).collect();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if parts.starts_with(&["codex", "exec"]) && parts.get(2) != Some(&"--add-dir") {
            let mut expected = expected;
            expected.splice(
                2..2,
                [
                    OsString::from("--add-dir"),
                    OsString::from("/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV/outbox"),
                ],
            );
            return expected;
        }
        expected
    }

    fn home() -> Vec<EnvVar> {
        vec![
            EnvVar::new("PATH", "/usr/bin"),
            EnvVar::new("CODEX_HOME", "/home/operator/.codex"),
        ]
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn own_outbox_is_an_exec_option_before_fresh_or_resumed_session_arguments() {
        for session in [
            TurnSession::Assigned,
            TurnSession::Resume(SessionId::new(THREAD)),
        ] {
            let spec = launch(&session, &[EnvVar::new("DUDES_MAILBOX", "/other/agent")])
                .expect("a launch");
            assert_eq!(
                &spec.argv[..4],
                argv(&[
                    "codex",
                    "exec",
                    "--add-dir",
                    "/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV/outbox",
                ])
            );
            if matches!(session, TurnSession::Resume(_)) {
                assert_eq!(&spec.argv[4..6], argv(&["resume", THREAD]));
            }
            assert_eq!(&spec.argv[spec.argv.len() - 2..], argv(&["--", TASK]));
        }
    }

    #[test]
    fn a_first_turn_names_no_session_and_carries_the_task_as_one_element() {
        let spec = launch(&TurnSession::Assigned, &home()).expect("a launch");
        assert_eq!(
            spec.argv,
            argv(&[
                "codex",
                "exec",
                "--json",
                "--ignore-user-config",
                "--ignore-rules",
                "-c",
                "features.plugins=false",
                "-c",
                "features.apps=false",
                "-c",
                "features.shell_snapshot=false",
                "--skip-git-repo-check",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "--",
                TASK,
            ])
        );
        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
        assert_eq!(spec.cwd, Path::new("/repo"));
    }

    #[test]
    fn a_resumed_turn_runs_exec_resume_on_the_confirmed_thread() {
        let spec = launch(&TurnSession::Resume(SessionId::new(THREAD)), &home()).expect("a launch");
        assert_eq!(
            spec.argv,
            argv(&[
                "codex",
                "exec",
                "resume",
                THREAD,
                "--json",
                "--ignore-user-config",
                "--ignore-rules",
                "-c",
                "features.plugins=false",
                "-c",
                "features.apps=false",
                "-c",
                "features.shell_snapshot=false",
                "--skip-git-repo-check",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "--",
                TASK,
            ])
        );
        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
    }

    /// The declarations of the Codex tests: the shared fixture plus an http server whose URL
    /// holds a quote, a brace and a newline, which the TOML serializer must keep inside one string.
    fn declared(chosen: &[&str]) -> Vec<McpLaunch> {
        let file = format!(
            "{}\n[odd]\nurl = \"https://odd.example/p?q=\\\"}}\\n\"\n",
            crate::mcp::fixture::FILE
        );
        crate::mcp::LoadedServers::new(
            &crate::mcp::fixture::declared(&file),
            &crate::mcp::fixture::planted(),
            |_| None,
        )
        .admit(
            &crate::mcp::fixture::names(chosen),
            crate::mcp::Rules {
                support: McpSupport::Http,
                default_isolation: true,
                trusted: false,
            },
        )
        .expect("http servers are admitted on Codex")
    }

    /// Each `mcp_servers.<name>` value read back as TOML, by name.
    fn tables(argv: &[OsString]) -> Vec<(String, toml::Table)> {
        argv.windows(2)
            .filter(|pair| pair[0] == "-c")
            .filter_map(|pair| {
                let option = pair[1].to_str()?;
                let (key, value) = option.split_once('=')?;
                let name = key.strip_prefix("mcp_servers.")?;
                let table: toml::Table =
                    toml::from_str(&format!("v = {value}")).expect("one TOML value");
                Some((name.to_owned(), table["v"].as_table()?.clone()))
            })
            .collect()
    }

    /// Opted in, exec and app-server each gain exactly one `-c` per server, in name order, and each
    /// value reads back as the literal table, operator text included. Mutants: drop
    /// `default_tools_approval_mode`, format the table by hand so the URL's quote ends the string,
    /// or name the operator's variable instead of the alias.
    #[test]
    fn each_declared_server_is_one_toml_table_on_both_transports() {
        let servers = declared(&["search", "odd", "memory"]);
        let env = home();
        let context = AgentCtx {
            mcp_servers: &servers,
            ..ctx(&env)
        };
        let selection = ModelSelection::provider_default();
        let exec = CodexAdapter::default()
            .managed(&context, TASK, &TurnSession::Assigned, &selection)
            .expect("a launch");
        let streaming = CodexAdapter::new(CodexConfig {
            streaming: true,
            ..CodexConfig::default()
        })
        .with_managed_home("/data/providers/codex".into())
        .managed(&context, TASK, &TurnSession::Assigned, &selection)
        .expect("a launch");
        let approve = || toml::Value::from("approve");
        let want = vec![
            (
                "memory".to_owned(),
                toml::Table::from_iter([
                    (
                        "url".to_owned(),
                        toml::Value::from("https://memory.example.com/mcp"),
                    ),
                    (
                        "env_http_headers".to_owned(),
                        toml::Value::Table(toml::Table::from_iter([(
                            "Authorization".to_owned(),
                            toml::Value::from("DUDES_MCP_MEMORY_HEADER_0"),
                        )])),
                    ),
                    ("default_tools_approval_mode".to_owned(), approve()),
                ]),
            ),
            (
                "odd".to_owned(),
                toml::Table::from_iter([
                    (
                        "url".to_owned(),
                        toml::Value::from("https://odd.example/p?q=\"}\n"),
                    ),
                    ("default_tools_approval_mode".to_owned(), approve()),
                ]),
            ),
            (
                "search".to_owned(),
                toml::Table::from_iter([
                    (
                        "url".to_owned(),
                        toml::Value::from("http://127.0.0.1:8931/mcp"),
                    ),
                    (
                        "bearer_token_env_var".to_owned(),
                        toml::Value::from("DUDES_MCP_SEARCH_BEARER"),
                    ),
                    ("default_tools_approval_mode".to_owned(), approve()),
                ]),
            ),
        ];
        assert_eq!(tables(&exec.argv), want);
        assert_eq!(tables(&streaming.argv), want);

        let plain = launch(&TurnSession::Assigned, &home()).expect("a launch");
        let sandbox = exec
            .argv
            .iter()
            .position(|part| part == "sandbox_mode=\"workspace-write\"")
            .expect("exec selects its sandbox");
        assert_eq!(exec.argv[..=sandbox], plain.argv[..=sandbox]);
        assert_eq!(exec.argv.len(), plain.argv.len() + 6);
        assert_eq!(exec.argv[exec.argv.len() - 2..], argv(&["--", TASK]));
        assert_eq!(
            exec.env,
            [
                EnvVar::new("CODEX_HOME", "/home/operator/.codex"),
                EnvVar::new(
                    "DUDES_MCP_MEMORY_HEADER_0",
                    crate::mcp::fixture::MEMORY_AUTH
                ),
                EnvVar::new("DUDES_MCP_SEARCH_BEARER", crate::mcp::fixture::SEARCH_KEY),
                EnvVar::new("PATH", "/usr/bin"),
            ]
        );
    }

    /// Codex starts a program server in a process group of its own, so its launch refuses one
    /// even if admission were bypassed.
    #[test]
    fn a_program_server_is_refused_at_the_launch() {
        let servers = crate::mcp::fixture::servers(&["notes"]);
        let context = AgentCtx {
            mcp_servers: &servers,
            ..ctx(&[])
        };
        assert_eq!(
            CodexAdapter::default().managed(
                &context,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default()
            ),
            Err(LaunchError::McpServer(McpRefusal::ProgramUnsupported(
                crate::mcp::fixture::names(&["notes"]).remove(0)
            )))
        );
        assert_eq!(CodexAdapter::default().mcp_support(), McpSupport::Http);
    }

    #[test]
    fn a_minted_session_is_refused() {
        assert_eq!(
            launch(&TurnSession::First(SessionId::new(THREAD)), &home()),
            Err(LaunchError::SessionModeMismatch)
        );
    }

    #[test]
    fn the_child_inherits_the_daemons_codex_home_and_nothing_sets_one() {
        let spec = launch(&TurnSession::Assigned, &home()).expect("a launch");
        assert_eq!(
            spec.env,
            [
                EnvVar::new("CODEX_HOME", "/home/operator/.codex"),
                EnvVar::new("PATH", "/usr/bin"),
            ]
        );

        let without =
            launch(&TurnSession::Assigned, &[EnvVar::new("PATH", "/usr/bin")]).expect("a launch");
        assert_eq!(without.env, [EnvVar::new("PATH", "/usr/bin")]);
    }

    #[test]
    fn a_named_model_follows_the_flags_before_the_task() {
        let ctx = ctx(&[]);
        let spec = CodexAdapter::default()
            .managed(
                &ctx,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5-codex"),
            )
            .expect("a launch");
        assert_eq!(
            spec.argv[spec.argv.len() - 4..],
            argv(&["-m", "gpt-5-codex", "--", TASK])
        );
    }

    #[test]
    fn an_empty_task_or_one_carrying_nul_is_refused() {
        let ctx = ctx(&[]);
        let adapter = CodexAdapter::default();
        let selection = ModelSelection::provider_default();
        assert_eq!(
            adapter.managed(&ctx, "", &TurnSession::Assigned, &selection),
            Err(LaunchError::EmptyTask)
        );
        assert_eq!(
            adapter.managed(&ctx, "a\0b", &TurnSession::Assigned, &selection),
            Err(LaunchError::TaskContainsNul { offset: 1 })
        );
    }

    #[test]
    fn it_declares_an_assigned_session_named_at_the_start_of_the_turn() {
        assert_eq!(
            CodexAdapter::default().session_acquisition(),
            SessionAcquisition::Assigned {
                resume: "resume",
                arrival: Arrival::Start,
            }
        );
    }

    #[test]
    fn the_billing_credentials_are_the_three_the_vendor_names() {
        assert_eq!(
            CodexAdapter::default().billing_env_vars(),
            ["CODEX_API_KEY", "OPENAI_API_KEY", "CODEX_ACCESS_TOKEN"]
        );
    }
    #[test]
    fn opt_in_streaming_keeps_the_task_off_argv_and_requires_the_daemon_home() {
        let config = CodexConfig {
            streaming: true,
            ..CodexConfig::default()
        };
        let adapter = CodexAdapter::new(config);
        let selection = ModelSelection::provider_default();
        assert_eq!(
            adapter.managed(&ctx(&[]), TASK, &TurnSession::Assigned, &selection),
            Err(LaunchError::ManagedProfileUnavailable)
        );
        let adapter = adapter.with_managed_home("/data/providers/codex".into());
        let launch = adapter
            .managed(&ctx(&[]), TASK, &TurnSession::Assigned, &selection)
            .unwrap();
        assert_eq!(
            launch.argv,
            argv(&[
                "codex",
                "app-server",
                "--listen",
                "stdio://",
                "--strict-config",
                "-c",
                "features.hooks=false",
                "-c",
                "features.plugins=false",
                "-c",
                "features.apps=false",
                "-c",
                "features.shell_snapshot=false",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "sandbox_workspace_write.exclude_tmpdir_env_var=true",
                "-c",
                "sandbox_workspace_write.exclude_slash_tmp=true",
                "-c",
                "sandbox_workspace_write.network_access=false",
            ])
        );
        assert!(!launch.argv.iter().any(|arg| arg == TASK));
        assert!(matches!(
            launch.stdin,
            crate::launch::LaunchInput::Protocol(_)
        ));
        assert_eq!(adapter.isolation(), Isolation::ConfigDir);
        let mut reader = adapter.turn_reader_for(&launch);
        assert_eq!(reader.start().len(), 1);
    }
}
