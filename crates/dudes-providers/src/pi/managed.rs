//! Pi, managed mode: the launch of one turn.
//!
//! A turn is `pi --mode json`, the isolation flags, `--session-dir` naming the agent's own session
//! directory, `--session <id>` when Pi confirmed a session, then `--` and the task as one argv
//! element. The plain launch is verified on pi 0.85.1 and 1.0.4. Declared HTTP servers add
//! one owned extension and a fixed server set on an exactly tested Pi release.
//!
//! # Isolation
//!
//! `--no-extensions`, `--no-skills` and `--no-prompt-templates` turn off discovery of the
//! extensions, skills and prompt templates in Pi's config directory and in `~/.agents/skills`, so
//! the operator's own set stays out of the turn and no program of the operator's runs inside Pi.
//! `--no-approve` makes Pi ignore the repository's `.pi` directory (settings, extensions, skills,
//! prompts and system prompt files) even when the operator's settings trust projects by default.
//! Each flag was measured against a planted resource on 2026-09-28: without it, the resource
//! loads. Context files (`AGENTS.md`, `CLAUDE.md`) still load, because they are the repository's
//! working notes, as they are for a Codex turn.
//!
//! # Sessions
//!
//! Every turn of an agent passes the same `--session-dir`, under the agent's own directory, so Pi
//! stores and looks up sessions there and nowhere else. `--session <id>` matches an exact id first
//! and then a prefix, and only this agent's sessions are in that directory; Pi names each session
//! with a version 7 UUID, and one of those is never a strict prefix of another. When the session's
//! file is gone, Pi finds nothing and exits 1 without running, so a lost conversation fails the
//! turn rather than silently starting a new one. `--session-id` is not used for resume, because on
//! a missing file it creates a new session under the same id and exits 0.
//!
//! One case remains. A session in that directory whose header names another working directory
//! makes Pi ask on stdin whether to fork it. Stdin is the null device, so Pi reads end of file and
//! exits 0 at once without running, and the reader finds no result.
//!
//! # The task
//!
//! After `--`, Pi still reads an argument that starts with `@` as a file to attach, and one that
//! starts with `/` can name a built-in command. Both receive one leading space, so Pi reads them as
//! text. A task that starts with `-` reaches Pi as text.
//! Exactly one task follows `--`, because Pi prompts once per message and settles after each, and
//! the reader ends the turn at the first settle.
//!
//! # The environment
//!
//! The daemon's environment passes through verbatim, sorted by name. The child inherits
//! `PI_CODING_AGENT_DIR`, the config directory that holds Pi's login and default model, and the
//! adapter never sets it. `--session-dir` overrides `PI_CODING_AGENT_SESSION_DIR` and any session
//! directory in Pi's settings. An opted-in launch receives only its server aliases and disables
//! the extension loader's filesystem cache; a plain launch keeps its environment unchanged.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use dudes_types::{ModelSelection, Provider, Tier};

use crate::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use crate::launch::{EnvVar, LaunchError, LaunchSpec};
use crate::mcp::{HttpAuth, McpServer, McpSupport};
use crate::pi::SESSION_ACQUISITION;
use crate::pi::config::PiConfig;
use crate::pi::stream::TurnStream;
use crate::stream::TurnReader;

const ISOLATION: &[&str] = &[
    "--no-extensions",
    "--no-skills",
    "--no-prompt-templates",
    "--no-approve",
];

const SESSION_DIR: &str = "pi-sessions";

/// Environment variables that select a Pi billing credential when present and non-empty.
///
/// Every name pi-ai 1.0.4's `getApiKeyEnvVars` maps a provider to (`dist/env-api-keys.js`),
/// plus `AWS_BEARER_TOKEN_BEDROCK`, the one variable that is a whole Amazon Bedrock credential on
/// its own. Which provider a turn uses comes from Pi's settings, which a pure adapter cannot read,
/// so every provider's key is a member.
///
/// Routes that bill with no single name here stay open, as they do for Claude: a login stored in
/// Pi's config directory, an `apiKey` written in its `models.json`, the AWS credential chain
/// (`AWS_PROFILE`, a key pair, container and web-identity credentials) and Google's
/// application-default credentials.
pub(crate) const BILLING_ENV_VARS: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "COPILOT_GITHUB_TOKEN",
    "ANT_LING_API_KEY",
    "QWEN_TOKEN_PLAN_API_KEY",
    "QWEN_TOKEN_PLAN_CN_API_KEY",
    "OPENAI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "NVIDIA_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_CLOUD_API_KEY",
    "GROQ_API_KEY",
    "CEREBRAS_API_KEY",
    "XAI_API_KEY",
    "RADIUS_API_KEY",
    "OPENROUTER_API_KEY",
    "AI_GATEWAY_API_KEY",
    "ZAI_API_KEY",
    "ZAI_CODING_CN_API_KEY",
    "MISTRAL_API_KEY",
    "MINIMAX_API_KEY",
    "MINIMAX_CN_API_KEY",
    "MOONSHOT_API_KEY",
    "HF_TOKEN",
    "FIREWORKS_API_KEY",
    "TOGETHER_API_KEY",
    "BASETEN_API_KEY",
    "OPENCODE_API_KEY",
    "KIMI_API_KEY",
    "CLOUDFLARE_API_KEY",
    "XIAOMI_API_KEY",
    "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "META_API_KEY",
    "TYPESAFE_API_KEY",
];

/// The Pi adapter: pure, like every adapter.
#[derive(Clone, Debug, Default)]
pub struct PiAdapter {
    config: PiConfig,
}

impl PiAdapter {
    /// An adapter for the `[providers.pi]` table.
    #[must_use]
    pub const fn new(config: PiConfig) -> Self {
        Self { config }
    }
}

impl Adapter for PiAdapter {
    fn provider(&self) -> Provider {
        Provider::Pi
    }

    fn program(&self) -> &OsStr {
        OsStr::new(&self.config.command)
    }

    /// Program-tier launches leave the tier unchanged, and no attached launch exists.
    fn tier(&self) -> Tier {
        Tier::ProgramGoverned
    }

    fn isolation(&self) -> Isolation {
        Isolation::Flags(ISOLATION)
    }

    fn instruction_manifest(
        &self,
        ctx: &AgentCtx<'_>,
        mode: dudes_types::Mode,
        _selection: &ModelSelection,
    ) -> crate::instructions::InstructionDeclaration {
        if mode == dudes_types::Mode::Attached {
            return crate::instructions::InstructionDeclaration::missing(Provider::Pi, mode);
        }
        crate::instructions::pi_manifest(ctx)
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn mcp_support(&self) -> McpSupport {
        McpSupport::Http
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
        let mut argv = vec![
            OsString::from(&self.config.command),
            "--mode".into(),
            "json".into(),
        ];
        argv.extend(ISOLATION.iter().map(OsString::from));
        if !ctx.mcp_servers.is_empty() {
            argv.extend([
                "--no-mcp".into(),
                "-e".into(),
                ctx.agent_dir.join("pi-mcp.ts").into(),
                format!("--dudes-mcp-servers={}", mcp_config(ctx)?).into(),
            ]);
        }
        argv.push("--session-dir".into());
        argv.push(ctx.agent_dir.join(SESSION_DIR).into());
        argv.extend(SESSION_ACQUISITION.argv(session)?);
        if let Some(model) = selection.selector() {
            argv.extend(["--model", model].map(OsString::from));
        }
        let mut text = OsString::new();
        if task.starts_with(['@', '/']) {
            text.push(" ");
        }
        text.push(task);
        argv.extend(["--".into(), text]);
        let mut env = self.child_env(ctx.daemon_env);
        if !ctx.mcp_servers.is_empty() {
            env.retain(|var| {
                if cfg!(windows) {
                    !var.name()
                        .as_encoded_bytes()
                        .eq_ignore_ascii_case(b"JITI_FS_CACHE")
                } else {
                    var.name() != OsStr::new("JITI_FS_CACHE")
                }
            });
            env.push(EnvVar::new("JITI_FS_CACHE", "false"));
            env = crate::mcp::with_secrets(env, ctx.mcp_servers);
        }
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

    fn managed_install(&self, ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall {
            directories: vec![ctx.agent_dir.join(SESSION_DIR)],
            files: if ctx.mcp_servers.is_empty() {
                Vec::new()
            } else {
                vec![(
                    ctx.agent_dir.join("pi-mcp.ts"),
                    include_str!("mcp-extension.ts").to_owned(),
                )]
            },
        }
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }

    /// Pi has no sandbox of its own.
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        BILLING_ENV_VARS
    }
}

fn mcp_config(ctx: &AgentCtx<'_>) -> Result<String, LaunchError> {
    let log = ctx.agent_dir.join("pi-mcp.log");
    let log = log.to_str().ok_or(LaunchError::PiMcpPathEncoding)?;
    let mut servers = BTreeMap::new();
    for server in ctx.mcp_servers {
        let McpServer::Http(http) = &server.server else {
            return Err(LaunchError::McpServer(
                crate::mcp::McpRefusal::ProgramUnsupported(server.name.clone()),
            ));
        };
        let headers: BTreeMap<String, String> = match &http.auth {
            HttpAuth::None => BTreeMap::new(),
            HttpAuth::Bearer(_) => BTreeMap::from([(
                "Authorization".to_owned(),
                format!("Bearer ${{{}}}", crate::mcp::bearer_alias(&server.name)),
            )]),
            HttpAuth::Headers(headers) => headers
                .keys()
                .enumerate()
                .map(|(position, name)| {
                    (
                        name.clone(),
                        format!("${{{}}}", crate::mcp::header_alias(&server.name, position)),
                    )
                })
                .collect(),
        };
        let mut value = serde_json::json!({"url":http.url,"exposure":"direct"});
        if !headers.is_empty() {
            value["headers"] = serde_json::json!(headers);
        }
        servers.insert(server.name.as_str(), value);
    }
    Ok(serde_json::json!({"logPath":log,"servers":servers}).to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dudes_types::{AgentId, SessionId};

    use super::*;
    use crate::adapter::Arrival;
    use crate::launch::EnvVar;

    const TASK: &str = "make the tests pass";
    const SESSION: &str = "3d8f310f-9cb5-7185-a385-54c0378f2355";
    const AGENT_DIR: &str = if cfg!(windows) {
        r"C:\dudes\agents\01ARZ3NDEKTSV4RRFFQ69G5FAV"
    } else {
        "/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"
    };
    const SESSION_DIR_PATH: &str = if cfg!(windows) {
        r"C:\dudes\agents\01ARZ3NDEKTSV4RRFFQ69G5FAV\pi-sessions"
    } else {
        "/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV/pi-sessions"
    };

    fn ctx(env: &[EnvVar]) -> AgentCtx<'_> {
        AgentCtx {
            agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid"),
            cwd: Path::new("/repo"),
            agent_dir: Path::new(AGENT_DIR),
            daemon_env: env,
            sandbox: None,
            trusted: false,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        }
    }

    fn launch_task(task: &str, session: &TurnSession) -> Result<LaunchSpec, LaunchError> {
        PiAdapter::default().managed(
            &ctx(&[]),
            task,
            session,
            &ModelSelection::provider_default(),
        )
    }

    fn launch(session: &TurnSession) -> Result<LaunchSpec, LaunchError> {
        launch_task(TASK, session)
    }

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// What follows the first `--`: Pi prompts once for each element.
    fn after_separator(spec: &LaunchSpec) -> &[OsString] {
        let at = spec
            .argv
            .iter()
            .position(|part| part == "--")
            .expect("a separator");
        &spec.argv[at + 1..]
    }

    #[test]
    fn declared_http_servers_add_only_the_owned_extension_and_aliases() {
        assert_eq!(
            PiAdapter::default().mcp_support(),
            crate::mcp::McpSupport::Http
        );
        let servers = crate::mcp::fixture::servers(&["search", "memory"]);
        let context = AgentCtx {
            mcp_servers: &servers,
            ..ctx(&[])
        };
        let adapter = PiAdapter::default();
        let spec = adapter
            .managed(
                &context,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default(),
            )
            .unwrap();
        let value = serde_json::json!({
            "logPath": Path::new(AGENT_DIR).join("pi-mcp.log").to_str().unwrap(),
            "servers": {
                "memory": {"url":"https://memory.example.com/mcp", "exposure":"direct", "headers":{"Authorization":"${DUDES_MCP_MEMORY_HEADER_0}"}},
                "search": {"url":"http://127.0.0.1:8931/mcp", "exposure":"direct", "headers":{"Authorization":"Bearer ${DUDES_MCP_SEARCH_BEARER}"}},
            }
        }).to_string();
        assert_eq!(
            spec.argv,
            vec![
                OsString::from("pi"),
                "--mode".into(),
                "json".into(),
                "--no-extensions".into(),
                "--no-skills".into(),
                "--no-prompt-templates".into(),
                "--no-approve".into(),
                "--no-mcp".into(),
                "-e".into(),
                Path::new(AGENT_DIR).join("pi-mcp.ts").into(),
                format!("--dudes-mcp-servers={value}").into(),
                "--session-dir".into(),
                Path::new(SESSION_DIR_PATH).into(),
                "--".into(),
                TASK.into()
            ]
        );
        assert_eq!(
            spec.env,
            vec![
                EnvVar::new(
                    "DUDES_MCP_MEMORY_HEADER_0",
                    crate::mcp::fixture::MEMORY_AUTH
                ),
                EnvVar::new("DUDES_MCP_SEARCH_BEARER", crate::mcp::fixture::SEARCH_KEY),
                EnvVar::new("JITI_FS_CACHE", "false"),
            ]
        );
        let install = adapter.managed_install(&context, &ModelSelection::provider_default());
        assert_eq!(install.directories, [Path::new(SESSION_DIR_PATH)]);
        assert_eq!(
            install.files,
            [(
                Path::new(AGENT_DIR).join("pi-mcp.ts"),
                include_str!("mcp-extension.ts").to_owned()
            )]
        );
    }

    #[test]
    fn declared_program_servers_are_still_refused() {
        let servers = crate::mcp::fixture::servers(&["notes"]);
        assert_eq!(
            PiAdapter::default().managed(
                &AgentCtx {
                    mcp_servers: &servers,
                    ..ctx(&[])
                },
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default()
            ),
            Err(LaunchError::McpServer(
                crate::mcp::McpRefusal::ProgramUnsupported(
                    dudes_types::McpServerName::parse("notes").unwrap()
                )
            ))
        );
    }

    #[test]
    fn plain_install_and_cache_environment_are_unchanged() {
        let env = [
            EnvVar::new("JITI_FS_CACHE", "true"),
            EnvVar::new("PI_CODING_AGENT_DIR", "/operator/pi"),
        ];
        let adapter = PiAdapter::default();
        let context = ctx(&env);
        let spec = adapter
            .managed(
                &context,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default(),
            )
            .unwrap();
        assert_eq!(spec.env, env);
        assert_eq!(
            adapter.managed_install(&context, &ModelSelection::provider_default()),
            ManagedInstall {
                directories: vec![Path::new(SESSION_DIR_PATH).to_path_buf()],
                files: Vec::new()
            }
        );
    }

    #[test]
    fn operator_url_bytes_round_trip_and_never_become_header_values() {
        let mut servers = crate::mcp::fixture::servers(&["memory"]);
        let McpServer::Http(http) = &mut servers[0].server else {
            panic!("the fixture is http")
        };
        http.url = "https://memory.example/mcp?quote=\"}\n".to_owned();
        let context = AgentCtx {
            mcp_servers: &servers,
            ..ctx(&[])
        };
        let spec = PiAdapter::default()
            .managed(
                &context,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default(),
            )
            .unwrap();
        let raw = spec
            .argv
            .iter()
            .filter_map(|arg| arg.to_str())
            .find_map(|arg| arg.strip_prefix("--dudes-mcp-servers="))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            value["servers"]["memory"]["url"],
            "https://memory.example/mcp?quote=\"}\n"
        );
        assert_eq!(
            value["servers"]["memory"]["headers"],
            serde_json::json!({"Authorization":"${DUDES_MCP_MEMORY_HEADER_0}"})
        );
        assert!(value["servers"]["memory"].get("command").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_agent_path_is_refused_only_when_mcp_json_needs_it() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new(OsStr::from_bytes(b"/agents/\xff"));
        let mut context = ctx(&[]);
        context.agent_dir = path;
        let adapter = PiAdapter::default();
        assert!(
            adapter
                .managed(
                    &context,
                    TASK,
                    &TurnSession::Assigned,
                    &ModelSelection::provider_default()
                )
                .is_ok()
        );
        let servers = crate::mcp::fixture::servers(&["memory"]);
        context.mcp_servers = &servers;
        assert_eq!(
            adapter.managed(
                &context,
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default()
            ),
            Err(LaunchError::PiMcpPathEncoding)
        );
    }

    #[test]
    fn a_first_turn_names_no_session_and_carries_the_task_as_one_element() {
        let spec = launch(&TurnSession::Assigned).expect("a launch");
        assert_eq!(
            spec.argv,
            argv(&[
                "pi",
                "--mode",
                "json",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-approve",
                "--session-dir",
                SESSION_DIR_PATH,
                "--",
                TASK,
            ])
        );
        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
        assert_eq!(spec.cwd, Path::new("/repo"));
    }

    #[test]
    fn a_resumed_turn_names_the_confirmed_session_in_the_same_directory() {
        let spec = launch(&TurnSession::Resume(SessionId::new(SESSION))).expect("a launch");
        assert_eq!(
            spec.argv,
            argv(&[
                "pi",
                "--mode",
                "json",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-approve",
                "--session-dir",
                SESSION_DIR_PATH,
                "--session",
                SESSION,
                "--",
                TASK,
            ])
        );
        assert_eq!(spec.stdin, crate::launch::LaunchInput::Null);
    }

    #[test]
    fn a_minted_session_is_refused() {
        assert_eq!(
            launch(&TurnSession::First(SessionId::new(SESSION))),
            Err(LaunchError::SessionModeMismatch)
        );
    }

    #[test]
    fn a_task_that_starts_with_a_dash_is_the_one_element_after_the_separator() {
        for session in [
            TurnSession::Assigned,
            TurnSession::Resume(SessionId::new(SESSION)),
        ] {
            let spec = launch_task("--help me - fix it", &session).expect("a launch");
            assert_eq!(after_separator(&spec), argv(&["--help me - fix it"]));
        }
    }

    #[test]
    fn a_task_that_starts_with_an_at_sign_is_sent_after_one_space() {
        for (task, sent) in [
            ("@src/lib.rs fix it", " @src/lib.rs fix it"),
            ("@", " @"),
            (" @src/lib.rs", " @src/lib.rs"),
            ("read @src/lib.rs", "read @src/lib.rs"),
        ] {
            let spec = launch_task(task, &TurnSession::Assigned).expect("a launch");
            assert_eq!(after_separator(&spec), argv(&[sent]), "{task:?}");
        }
    }

    #[test]
    fn a_task_that_starts_with_a_slash_is_sent_after_one_space() {
        for session in [
            TurnSession::Assigned,
            TurnSession::Resume(SessionId::new(SESSION)),
        ] {
            for (task, sent) in [
                ("/llama", " /llama"),
                ("/llama use the local model", " /llama use the local model"),
                ("/llamafile", " /llamafile"),
                ("/llama\ttext", " /llama\ttext"),
                ("/model gpt", " /model gpt"),
                ("/", " /"),
                ("/src/lib.rs is broken", " /src/lib.rs is broken"),
                (" /llama", " /llama"),
                ("read /llama", "read /llama"),
            ] {
                let spec = launch_task(task, &session).expect("a launch");
                assert_eq!(after_separator(&spec), argv(&[sent]), "{task:?}");
            }
        }
    }

    #[test]
    fn an_empty_task_or_one_carrying_nul_is_refused() {
        assert_eq!(
            launch_task("", &TurnSession::Assigned),
            Err(LaunchError::EmptyTask)
        );
        assert_eq!(
            launch_task("a\0b", &TurnSession::Assigned),
            Err(LaunchError::TaskContainsNul { offset: 1 })
        );
    }

    #[test]
    fn the_child_inherits_the_daemons_environment_and_nothing_sets_pis_config_directory() {
        let env = [
            EnvVar::new("PI_CODING_AGENT_DIR", "/home/operator/.pi/agent"),
            EnvVar::new("PATH", "/usr/bin"),
        ];
        let spec = PiAdapter::default()
            .managed(
                &ctx(&env),
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::provider_default(),
            )
            .expect("a launch");
        assert_eq!(
            spec.env,
            [
                EnvVar::new("PATH", "/usr/bin"),
                EnvVar::new("PI_CODING_AGENT_DIR", "/home/operator/.pi/agent"),
            ]
        );
        assert_eq!(
            launch(&TurnSession::Assigned).expect("a launch").env,
            Vec::<EnvVar>::new()
        );
    }

    #[test]
    fn a_named_model_follows_the_flags_before_the_task() {
        let spec = PiAdapter::default()
            .managed(
                &ctx(&[]),
                TASK,
                &TurnSession::Assigned,
                &ModelSelection::named("openai-codex/gpt-5.5"),
            )
            .expect("a launch");
        assert_eq!(
            spec.argv[spec.argv.len() - 4..],
            argv(&["--model", "openai-codex/gpt-5.5", "--", TASK])
        );
    }

    #[test]
    fn it_declares_an_assigned_session_named_at_the_start_of_the_turn() {
        assert_eq!(
            PiAdapter::default().session_acquisition(),
            SessionAcquisition::Assigned {
                resume: "--session",
                arrival: Arrival::Start,
            }
        );
    }

    #[test]
    fn every_launch_carries_the_isolation_it_declares() {
        let adapter = PiAdapter::default();
        let Isolation::Flags(flags) = adapter.isolation() else {
            panic!("Pi isolates by flags: {:?}", adapter.isolation());
        };
        assert_eq!(
            flags,
            [
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-approve"
            ]
        );
        let spec = launch(&TurnSession::Assigned).expect("a launch");
        assert_eq!(spec.argv[3..7], argv(flags));
    }

    #[test]
    fn the_billing_credentials_are_the_provider_keys_pi_reads() {
        let names = PiAdapter::default().billing_env_vars();
        assert_eq!(names.len(), 39);
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "META_API_KEY",
            "TYPESAFE_API_KEY",
        ] {
            assert!(names.contains(&name), "{name}");
        }
        for name in ["PI_CODING_AGENT_DIR", "AWS_PROFILE", "AWS_REGION"] {
            assert!(!names.contains(&name), "{name} selects no credential");
        }
    }

    /// `pi auth check` needs a provider or model from Pi's own settings and refreshes OAuth, so
    /// the daemon has no status command to run for Pi.
    #[test]
    fn pi_has_no_sign_in_check_while_claude_and_codex_do() {
        assert_eq!(PiAdapter::default().sign_in_check(), None);
        assert_eq!(
            crate::claude::ClaudeAdapter::default().sign_in_check(),
            Some(&["auth", "status"][..])
        );
        assert_eq!(
            crate::codex::CodexAdapter::default().sign_in_check(),
            Some(&["login", "status"][..])
        );
    }
}
