use std::collections::BTreeMap;
use std::ffi::OsString;

use dudes_providers::scoped::ScopedModelProfile;

use super::*;

const MACOS_SHA: &str = "85d8c08707fcfd1e78ffa0204eefbc6ec9e4fe22f375fb2767ef85d957ed3161";
const LINUX_SHA: &str = "86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada";

pub(super) fn prepare(
    registry: &PrivateRoots,
    launch: ScopedManagedLaunch,
    attempt: Attempt,
) -> io::Result<PreparedScopedLaunch> {
    let sha = qualification(launch.inventory.platform, &launch.inventory.private.temp)?;
    #[cfg(test)]
    let test_sha = registry.claude_test_sha.lock().unwrap().clone();
    #[cfg(test)]
    let sha = test_sha.as_deref().unwrap_or(sha);
    prepare_pinned(registry, launch, attempt, sha)
}

fn qualification(platform: ScopedPlatform, temp: &Path) -> io::Result<&'static str> {
    match platform {
        ScopedPlatform::MacOsArm64 if cfg!(all(target_os = "macos", target_arch = "aarch64")) => {
            Ok(MACOS_SHA)
        }
        ScopedPlatform::LinuxX86_64 if cfg!(all(target_os = "linux", target_arch = "x86_64")) => {
            linux_temp(temp)?;
            Ok(LINUX_SHA)
        }
        _ => Err(refusal(
            "scoped Claude executable has no qualified platform profile",
        )),
    }
}

fn linux_temp(temp: &Path) -> io::Result<()> {
    // Reserve space for the Linux bridge socket suffix and terminating NUL.
    if temp.as_os_str().len() > 72 {
        return Err(refusal(
            "scoped Claude temporary path exceeds its Linux socket limit",
        ));
    }
    Ok(())
}

fn prepare_pinned(
    registry: &PrivateRoots,
    launch: ScopedManagedLaunch,
    attempt: Attempt,
    sha: &str,
) -> io::Result<PreparedScopedLaunch> {
    let private = &launch.inventory.private;
    let agent_dir = private
        .outbox
        .parent()
        .ok_or_else(|| refusal("scoped Claude agent directory is unavailable"))?;
    if attempt.started == Lsn::ZERO
        || attempt.generation == 0
        || !matches!(attempt.owner, InstructionObservationOwner::Managed { agent, turn, .. }
            if turn == attempt.turn && agent_dir.file_name() == Some(agent.to_string().as_ref()))
        || private.profile != agent_dir.join("profile")
        || launch.unarmed.candidate_version != "2.1.311"
        || launch.unarmed.model_profile != ScopedModelProfile::ClaudeSonnet46
        || !matches!(&launch.spec.stdin, LaunchInput::WriteThenClose(task) if !task.is_empty())
    {
        return Err(refusal(
            "scoped Claude physical attempt or profile is unavailable",
        ));
    }
    validate_environment(&launch)?;
    let roots = registry.agent(agent_dir)?;
    let generation = roots.mutate()?;
    let deadline = Instant::now() + PROBE_DEADLINE;
    let program = launch
        .spec
        .argv
        .first()
        .map(Path::new)
        .ok_or_else(|| refusal("scoped Claude executable is unavailable"))?;
    let binary = PinnedFile::open(program, sha, None, deadline)?;
    if !launch.install.files.is_empty()
        || launch.install.directories
            != [
                private.home.clone(),
                private.profile.clone(),
                private.work.clone(),
                private.outbox.clone(),
                private.temp.clone(),
            ]
    {
        return Err(refusal(
            "scoped Claude install differs from its original plan",
        ));
    }
    for directory in &launch.install.directories {
        owner_directory(directory)?;
    }
    let inspection = Inspection::capture(&launch.inventory, deadline)?;
    let binding = Arc::new(ScopedBinding {
        generation: *generation,
        roots: Arc::clone(&roots),
        inspection,
        binary,
        spec: launch.spec.clone(),
        provider: ProviderBinding::Claude,
    });
    Ok(PreparedScopedLaunch {
        launch,
        attempt,
        binding,
    })
}

fn validate_environment(launch: &ScopedManagedLaunch) -> io::Result<()> {
    let p = &launch.inventory.private;
    let agent_dir = p
        .outbox
        .parent()
        .ok_or_else(|| refusal("scoped Claude agent directory is unavailable"))?;
    let mut expected = BTreeMap::from([
        ("HOME", p.home.as_os_str().to_owned()),
        ("CLAUDE_CONFIG_DIR", p.profile.as_os_str().to_owned()),
        ("DUDES_MAILBOX", agent_dir.as_os_str().to_owned()),
        ("XDG_CONFIG_HOME", p.home.join(".config").into_os_string()),
        ("XDG_CACHE_HOME", p.home.join(".cache").into_os_string()),
        ("XDG_STATE_HOME", p.home.join(".state").into_os_string()),
        ("CLAUDE_CODE_TMPDIR", p.temp.as_os_str().to_owned()),
        ("TMPDIR", p.temp.as_os_str().to_owned()),
        ("TMP", p.temp.as_os_str().to_owned()),
        ("TEMP", p.temp.as_os_str().to_owned()),
    ]);
    expected.extend(
        [
            ("DUDES_COMMAND_TRANSPORT", "file-mail"),
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
        .map(|(name, value)| (name, OsString::from(value))),
    );
    let mut seen = BTreeSet::new();
    for var in &launch.spec.env {
        let name = var
            .name()
            .to_str()
            .ok_or_else(|| refusal("scoped Claude environment name is invalid"))?;
        if !seen.insert(name) {
            return Err(refusal("scoped Claude environment is ambiguous"));
        }
        if let Some(value) = expected.remove(name) {
            if var.expose_value() != value {
                return Err(refusal("scoped Claude private environment changed"));
            }
        } else if !matches!(
            name,
            "PATH" | "LANG" | "ANTHROPIC_API_KEY" | "ANTHROPIC_BASE_URL"
        ) {
            return Err(refusal(
                "scoped Claude environment contains an unowned setting",
            ));
        }
    }
    if !expected.is_empty() {
        return Err(refusal("scoped Claude private environment is incomplete"));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
