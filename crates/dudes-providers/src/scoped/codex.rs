use std::ffi::OsString;
use std::path::Path;

use dudes_types::{ModelSelection, Provider};

use super::{
    ScopedError, ScopedLaunchInputs, ScopedManagedLaunch, ScopedProofGap, environment, inventory,
    unarmed,
};
use crate::adapter::{AgentCtx, ManagedInstall, TurnSession};
use crate::codex::config::CodexConfig;
use crate::codex::startup::CodexStartup;
use crate::launch::{EnvVar, LaunchError, LaunchInput, LaunchSpec};

pub(crate) fn compose(
    config: &CodexConfig,
    home: Option<&Path>,
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
    if !matches!(session, TurnSession::Assigned) {
        return Err(ScopedError::Session.into());
    }
    if !config.streaming {
        return Err(ScopedError::Options.into());
    }
    if home != Some(scope.private.profile.as_path()) {
        return Err(ScopedError::Paths.into());
    }
    let inventory = inventory(ctx, scope, selection, "gpt-5.5")?;
    let roots = inventory.additional_roots();
    let mut argv = vec![OsString::from(&config.command)];
    argv.extend(
        crate::codex::model_list::APP_SERVER_ARGS
            .iter()
            .map(OsString::from),
    );
    let roots_json = serde_json::to_string(&roots).expect("validated paths serialize");
    let quoted = |path: &Path| serde_json::to_string(path).expect("validated path serializes");
    let mut ancestors = std::collections::BTreeSet::new();
    for entry in scope.hired.repositories.entries() {
        ancestors.extend(
            entry
                .value
                .execution
                .directory
                .ancestors()
                .map(Path::to_path_buf),
        );
    }
    let projects = ancestors
        .iter()
        .map(|path| format!("{}={{trust_level=\"untrusted\"}}", quoted(path)))
        .collect::<Vec<_>>()
        .join(",");
    for option in [
        "features.multi_agent=false".to_owned(),
        // Only the daemon admits another task; native goals can start idle turns.
        "features.goals=false".to_owned(),
        "web_search=\"disabled\"".to_owned(),
        "mcp_servers={}".to_owned(),
        format!("projects={{{projects}}}"),
        format!("sqlite_home={}", quoted(&inventory.private.profile)),
        format!("log_dir={}", quoted(&inventory.private.profile.join("log"))),
        format!("sandbox_workspace_write.writable_roots={roots_json}"),
    ] {
        argv.extend(["-c".into(), option.into()]);
    }
    let mut env = environment(ctx, &inventory, Provider::Codex)?;
    env.extend([
        EnvVar::new("CODEX_HOME", &inventory.private.profile),
        EnvVar::new("SHELL", "/bin/bash"),
        EnvVar::new("TERM", "dumb"),
    ]);
    env.sort();
    let startup = CodexStartup::new(
        inventory.private.profile.clone(),
        ctx.agent_dir.to_owned(),
        inventory.cwd.clone(),
        task.to_owned(),
        TurnSession::Assigned,
        selection.clone(),
    )
    .with_scoped_roots(roots);
    let p = &inventory.private;
    let install = ManagedInstall {
        directories: vec![
            p.home.clone(),
            p.profile.clone(),
            p.work.clone(),
            p.outbox.clone(),
            p.temp.clone(),
        ],
        files: Vec::new(),
    };
    Ok(ScopedManagedLaunch {
        spec: LaunchSpec {
            argv,
            env,
            cwd: inventory.cwd.clone(),
            stdin: LaunchInput::Protocol(startup),
        },
        install,
        inventory,
        unarmed: unarmed("0.164.0-alpha.11.1", ScopedProofGap::CodexModelTools),
    })
}
