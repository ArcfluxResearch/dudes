use std::ffi::OsString;

use dudes_types::{IsolationMode, ModelSelection, Provider};
use serde_json::json;

use super::{
    ScopedError, ScopedLaunchInputs, ScopedManagedLaunch, ScopedProofGap, environment, inventory,
    unarmed,
};
use crate::adapter::{AgentCtx, ManagedInstall, TurnSession};
use crate::claude::config::{ClaudeConfig, PermissionMode};
use crate::launch::{EnvVar, LaunchError, LaunchInput, LaunchSpec};

pub(crate) fn compose(
    config: &ClaudeConfig,
    ctx: &AgentCtx<'_>,
    scope: &ScopedLaunchInputs<'_>,
    task: &str,
    session: &TurnSession,
    selection: &ModelSelection,
) -> Result<ScopedManagedLaunch, LaunchError> {
    if task.is_empty() {
        return Err(LaunchError::EmptyTask);
    }
    if let Some(offset) = task.find('\0') {
        return Err(LaunchError::TaskContainsNul { offset });
    }
    let session_args = crate::claude::SESSION_ACQUISITION
        .argv(session)
        .map_err(|_| ScopedError::Session)?;
    if config.permission_mode != PermissionMode::Default
        || config.isolation == IsolationMode::Inherit
        || config
            .setting_sources
            .as_ref()
            .is_some_and(|sources| !sources.is_empty())
        || !config.extra_args.is_empty()
        || !config.tools.is_empty()
        || !config.allowed_tools.is_empty()
        || !config.disallowed_tools.is_empty()
        || config
            .model
            .as_deref()
            .is_some_and(|model| selection.selector() != Some(model))
    {
        return Err(ScopedError::Options.into());
    }
    let inventory = inventory(ctx, scope, selection, "claude-sonnet-4-6")?;
    let roots = inventory.writable_roots();
    let text = |path: &std::path::Path| path.to_str().expect("validated scoped path").to_owned();
    let allow: Vec<_> = roots
        .iter()
        .map(|path| format!("Edit(/{}/**)", text(path)))
        .collect();
    let mut deny: Vec<_> = inventory
        .protected
        .iter()
        .flat_map(|path| {
            [
                format!("Edit(/{})", text(path)),
                format!("Edit(/{}/**)", text(path)),
            ]
        })
        .collect();
    deny.push("Bash(dangerouslyDisableSandbox:true)".to_owned());
    let settings = json!({
        "disableAllHooks":true,"autoMemoryEnabled":false,"disableClaudeAiConnectors":true,
        "permissions":{"defaultMode":"dontAsk","disableBypassPermissionsMode":"disable","disableAutoMode":"disable","allow":allow,"deny":deny},
        "sandbox":{"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false,"excludedCommands":[],"enableWeakerNestedSandbox":false,"enableWeakerNetworkIsolation":false,"allowAppleEvents":false,"allowNestedUserNamespaces":false,
            "filesystem":{"disabled":false,"allowWrite":roots,"denyWrite":inventory.protected},
            "network":{"allowedDomains":[],"strictAllowlist":true,"allowUnixSockets":[],"allowAllUnixSockets":false,"allowLocalBinding":false},
            "credentials":{"envVars":[{"name":"ANTHROPIC_API_KEY","mode":"deny"}]}}
    });
    let mut argv = vec![config.command.clone()];
    argv.extend(
        [
            "-p",
            "--input-format",
            "text",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--bare",
            "--restricted",
            "--permission-mode",
            "dontAsk",
            "--permission-prompts",
            "none",
            "--setting-sources",
            "",
        ]
        .map(OsString::from),
    );
    argv.extend([
        OsString::from("--settings"),
        OsString::from(settings.to_string()),
    ]);
    argv.extend(
        [
            "--tools",
            "Bash,Read,Edit",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--disable-slash-commands",
            "--no-chrome",
            "--model",
            "claude-sonnet-4-6",
        ]
        .map(OsString::from),
    );
    argv.extend(session_args);
    if let Some(turns) = ctx.max_turns {
        argv.extend(["--max-turns".into(), turns.to_string().into()]);
    }
    if let Some(budget) = ctx.max_budget {
        argv.extend(["--max-budget-usd".into(), budget.to_decimal().into()]);
    }
    argv.push("--add-dir".into());
    argv.extend(
        roots
            .iter()
            .filter(|root| **root != inventory.cwd)
            .map(|root| root.as_os_str().to_owned()),
    );
    let mut env = environment(ctx, &inventory, Provider::Claude)?;
    let private = &inventory.private;
    env.extend([
        EnvVar::new("CLAUDE_CONFIG_DIR", &private.profile),
        EnvVar::new("XDG_CONFIG_HOME", private.home.join(".config")),
        EnvVar::new("XDG_CACHE_HOME", private.home.join(".cache")),
        EnvVar::new("XDG_STATE_HOME", private.home.join(".state")),
        EnvVar::new("CLAUDE_CODE_TMPDIR", &private.temp),
    ]);
    env.extend(
        [
            ("SHELL", "/bin/bash"),
            ("CLAUDE_CODE_SHELL", "/bin/bash"),
            ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
            ("CLAUDE_CODE_DISABLE_POLICY_SKILLS", "1"),
            ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1"),
            ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
            ("CLAUDE_CODE_MAX_RETRIES", "0"),
            ("DISABLE_UPDATES", "1"),
            ("TERM", "dumb"),
        ]
        .map(|(name, value)| EnvVar::new(name, value)),
    );
    env.sort();
    let install = ManagedInstall {
        directories: vec![
            private.home.clone(),
            private.profile.clone(),
            private.work.clone(),
            private.outbox.clone(),
            private.temp.clone(),
        ],
        files: Vec::new(),
    };
    Ok(ScopedManagedLaunch {
        spec: LaunchSpec {
            argv,
            env,
            cwd: inventory.cwd.clone(),
            stdin: LaunchInput::WriteThenClose(task.as_bytes().to_vec()),
        },
        install,
        inventory,
        unarmed: unarmed("2.1.311", ScopedProofGap::ClaudePersistence),
    })
}
