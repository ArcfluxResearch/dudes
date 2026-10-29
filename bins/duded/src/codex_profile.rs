use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dudes_proto::rpc::system::ProviderLoginHome;
use dudes_providers::codex::startup::CodexStartup;
use dudes_providers::launch::{EnvVar, LaunchSpec};
#[cfg(target_os = "macos")]
use serde::Deserialize;

use crate::predicate::{PredicateOutcome, PredicateRunner};

pub(crate) mod scoped;

const PROBE_LIMIT: usize = 256;
const PROBE_DEADLINE: Duration = Duration::from_secs(2);
const FORBIDDEN_HOME_ENTRIES: &[&str] = &["config.toml", "rules", "profiles", "hooks.json"];
#[cfg(target_os = "macos")]
const MANAGED_PREFERENCES: &str = r#"ObjC.import("CoreFoundation");
function run() {
  const domain = $("com.openai.codex");
  const synchronized = Boolean($.CFPreferencesAppSynchronize(domain));
  if (!synchronized) throw new Error("Preference synchronization failed");
  return JSON.stringify({
    synchronized: synchronized,
    config: Boolean($.CFPreferencesAppValueIsForced($("config_toml_base64"), domain)),
    requirements: Boolean($.CFPreferencesAppValueIsForced($("requirements_toml_base64"), domain))
  });
}"#;

pub(crate) struct CodexProfile {
    home: PathBuf,
    env: Vec<EnvVar>,
    runner: Arc<dyn PredicateRunner>,
    mutations: Arc<scoped::PrivateRoots>,
    #[cfg(test)]
    scoped_test_qualification: Option<(String, String, usize)>,
}

impl fmt::Debug for CodexProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexProfile")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl CodexProfile {
    pub(crate) fn new(
        state_dir: &Path,
        env: &[EnvVar],
        runner: Arc<dyn PredicateRunner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            home: state_dir.join("providers/codex"),
            env: env.to_vec(),
            runner,
            mutations: Arc::default(),
            #[cfg(test)]
            scoped_test_qualification: None,
        })
    }

    pub(crate) fn login_home(&self, program: Option<&Path>) -> ProviderLoginHome {
        match program
            .ok_or_else(|| refusal("Codex executable is unavailable"))
            .and_then(|program| self.admit(program))
        {
            Ok(path) => ProviderLoginHome::Ready { path },
            Err(error) => ProviderLoginHome::Unavailable {
                reason: error.to_string(),
            },
        }
    }

    fn admit(&self, program: &Path) -> io::Result<PathBuf> {
        self.machine_policy()?;
        let home = self.check_home(true)?;
        let mut env = self.env.clone();
        set_env(&mut env, "CODEX_HOME", home.as_os_str().to_owned());
        env.retain(|var| !matches!(var.name().to_str(), Some("DUDES_TOKEN" | "DUDES_SOCK")));
        let spec = LaunchSpec {
            argv: vec![program.as_os_str().to_owned(), "--version".into()],
            env,
            cwd: home.clone(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        };
        dudes_providers::codex::version::check_cli(&self.probe(&spec)?)
            .map_err(|unsupported| io::Error::new(io::ErrorKind::PermissionDenied, unsupported))?;
        Ok(home)
    }

    pub(crate) fn prepare(
        &self,
        spec: &mut LaunchSpec,
        startup: &CodexStartup,
    ) -> io::Result<CodexStartup> {
        if startup.scoped_roots().is_some() {
            return Err(refusal(
                "scoped Codex launch requires its retained preparation owner",
            ));
        }
        let _mutation = self.mutations.mutate()?;
        let program = spec
            .argv
            .first()
            .ok_or_else(|| refusal("Codex executable is unavailable"))?;
        let home = self.admit(Path::new(program))?;
        if startup.home() != self.home {
            return Err(refusal("Codex profile does not belong to this daemon"));
        }
        owner_directory(startup.agent_dir())?;
        owner_directory(&startup.agent_dir().join("tmp"))?;
        let agent_dir = startup
            .agent_dir()
            .canonicalize()
            .map_err(|_| refusal("Codex agent directory cannot be resolved"))?;
        let prepared = startup.resolved_paths(
            home,
            agent_dir,
            startup
                .cwd()
                .canonicalize()
                .map_err(|_| refusal("Codex workspace cannot be resolved"))?,
        );
        Self::check_layout(&prepared)?;
        let ancestors = ancestors(prepared.cwd())?;
        let projects = ancestors
            .iter()
            .map(|path| {
                path.to_str()
                    .and_then(|path| serde_json::to_string(path).ok())
                    .map(|path| format!("{path} = {{ trust_level = \"untrusted\" }}"))
                    .ok_or_else(|| refusal("Codex project path is not supported"))
            })
            .collect::<io::Result<Vec<_>>>()?
            .join(", ");
        let home = quoted(prepared.home())?;
        let temp = quoted(&prepared.agent_dir().join("tmp"))?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let roots = format!("{temp},{}", quoted(&prepared.agent_dir().join("outbox"))?);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let roots = temp;
        let log = quoted(&prepared.home().join("log"))?;
        for option in [
            format!("projects={{ {projects} }}"),
            format!("sqlite_home={home}"),
            format!("log_dir={log}"),
            format!("sandbox_workspace_write.writable_roots=[{roots}]"),
        ] {
            spec.argv
                .extend([OsString::from("-c"), OsString::from(option)]);
        }
        set_env(
            &mut spec.env,
            "CODEX_HOME",
            prepared.home().as_os_str().to_owned(),
        );
        for name in ["TMPDIR", "TMP", "TEMP"] {
            set_env(
                &mut spec.env,
                name,
                prepared.agent_dir().join("tmp").into_os_string(),
            );
        }
        spec.env.retain(|var| {
            !var.name().to_str().is_some_and(|name| {
                name.starts_with("CODEX_INTERNAL_") || name == "CODEX_SQLITE_HOME"
            })
        });
        spec.env.sort();
        Ok(prepared)
    }

    pub(crate) fn before_task(&self, startup: &CodexStartup) -> io::Result<()> {
        let _inspection = self.mutations.inspect()?;
        self.machine_policy()?;
        if self.check_home(false)? != startup.home() {
            return Err(refusal("Codex profile path changed during startup"));
        }
        Self::check_layout(startup)
    }

    fn check_home(&self, create: bool) -> io::Result<PathBuf> {
        normalized_absolute(&self.home)?;
        protected_ancestors(&self.home)?;
        if create {
            owner_directory(&self.home)?;
        }
        let metadata = std::fs::symlink_metadata(&self.home)
            .map_err(|_| refusal("Codex managed home is unavailable"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(refusal("Codex managed home must be a real directory"));
        }
        for entry in FORBIDDEN_HOME_ENTRIES {
            absent(&self.home.join(entry))?;
        }
        self.home
            .canonicalize()
            .map_err(|_| refusal("Codex managed home cannot be resolved"))
    }

    fn check_layout(startup: &CodexStartup) -> io::Result<()> {
        let home = startup.home();
        let cwd = startup.cwd();
        normalized_absolute(cwd)?;
        normalized_absolute(startup.agent_dir())?;
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|_| refusal("Codex workspace cannot be resolved"))?;
        if startup.canonical_cwd() != Some(canonical_cwd.as_path()) {
            return Err(refusal("Codex workspace target changed during startup"));
        }
        let canonical_agent = startup
            .agent_dir()
            .canonicalize()
            .map_err(|_| refusal("Codex agent directory cannot be resolved"))?;
        let temp = startup.agent_dir().join("tmp");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let outbox = startup.agent_dir().join("outbox");
            protected_ancestors(&outbox)?;
            let metadata = std::fs::symlink_metadata(&outbox)
                .map_err(|_| refusal("Codex owned outbox is unavailable"))?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || outbox.canonicalize().ok().as_deref() != Some(outbox.as_path())
            {
                return Err(refusal(
                    "Codex owned outbox must be a real unchanged directory",
                ));
            }
        }
        if canonical_agent != startup.agent_dir()
            || temp.canonicalize().ok().as_deref() != Some(temp.as_path())
        {
            return Err(refusal(
                "Codex private temporary directory must not change or alias another path",
            ));
        }
        for root in [
            cwd,
            canonical_cwd.as_path(),
            startup.agent_dir(),
            temp.as_path(),
        ] {
            if home.starts_with(root) || root.starts_with(home) {
                return Err(refusal(
                    "Codex managed home overlaps a writable workspace or agent directory",
                ));
            }
        }
        Ok(())
    }

    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            clippy::unused_self,
            reason = "macOS metadata is queried through the owned bounded runner"
        )
    )]
    fn machine_policy(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            absent(Path::new("/etc/codex/managed_config.toml"))?;
            absent(Path::new("/etc/codex/requirements.toml"))?;
        }
        #[cfg(target_os = "macos")]
        {
            let spec = LaunchSpec {
                argv: [
                    "/usr/bin/osascript",
                    "-l",
                    "JavaScript",
                    "-e",
                    MANAGED_PREFERENCES,
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
                env: self
                    .env
                    .iter()
                    .filter(|var| matches!(var.name().to_str(), Some("HOME" | "LANG")))
                    .cloned()
                    .collect(),
                cwd: "/".into(),
                stdin: dudes_providers::launch::LaunchInput::Null,
            };
            let metadata: PreferenceMetadata = serde_json::from_str(&self.probe(&spec)?)
                .map_err(|_| refusal("Codex managed-policy metadata is unavailable"))?;
            if !metadata.synchronized || metadata.config || metadata.requirements {
                return Err(refusal(
                    "Codex streaming does not support managed machine policy",
                ));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(refusal(
            "Codex streaming profile admission is unsupported on this platform",
        ));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Ok(())
    }

    fn probe(&self, spec: &LaunchSpec) -> io::Result<String> {
        match self.runner.run(spec, PROBE_DEADLINE) {
            Ok(PredicateOutcome::Finished {
                exit: Some(0),
                output_tail,
                timed_out: false,
            }) if output_tail.len() <= PROBE_LIMIT => Ok(output_tail),
            _ => Err(refusal(
                "Codex profile metadata probe did not complete successfully",
            )),
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceMetadata {
    synchronized: bool,
    config: bool,
    requirements: bool,
}

fn refusal(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}
fn absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(refusal(
            "Codex streaming found configuration or rules outside its controlled profile",
        )),
        Err(_) => Err(refusal(
            "Codex streaming cannot inspect configuration metadata",
        )),
    }
}
pub(crate) fn normalized_absolute(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        || path.as_os_str().len() > 32 * 1024
    {
        return Err(refusal(
            "Codex streaming requires a normalized absolute path",
        ));
    }
    Ok(())
}
fn owner_directory(path: &Path) -> io::Result<()> {
    normalized_absolute(path)?;
    protected_ancestors(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(refusal(
            "Codex profile directories cannot be symbolic links",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| refusal("Codex profile directory cannot be created"))?;
        protected_ancestors(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| refusal("Codex profile directory cannot be restricted"))?;
    }
    #[cfg(not(unix))]
    return Err(refusal(
        "Codex profile directory protection is unsupported on this platform",
    ));
    #[cfg(unix)]
    Ok(())
}
#[cfg(unix)]
pub(crate) fn protected_ancestors(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let owner = nix::unistd::geteuid().as_raw();
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    enqueue_ancestors(path, &mut pending, &seen)?;
    while let Some(path) = pending.pop_first() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(refusal("Codex profile ancestry cannot be inspected")),
        };
        if metadata.uid() != owner && metadata.uid() != 0 {
            return Err(refusal("Codex profile ancestry is owned by another user"));
        }
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|_| refusal("Codex profile alias cannot be inspected"))?;
            let target = if target.is_absolute() {
                target
            } else {
                path.parent()
                    .ok_or_else(|| refusal("Codex profile alias has no parent"))?
                    .join(target)
            };
            enqueue_ancestors(&target, &mut pending, &seen)?;
        } else if !metadata.is_dir()
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err(refusal(
                "Codex profile ancestry is replaceable by another user",
            ));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| refusal("Codex profile alias cannot be resolved"))?;
        enqueue_ancestors(&canonical, &mut pending, &seen)?;
    }
    Ok(())
}

#[cfg(unix)]
fn enqueue_ancestors(
    path: &Path,
    pending: &mut BTreeSet<PathBuf>,
    seen: &BTreeSet<PathBuf>,
) -> io::Result<()> {
    for ancestor in path.ancestors() {
        if !seen.contains(ancestor) {
            pending.insert(ancestor.to_owned());
        }
        if seen.len() + pending.len() > 512 {
            return Err(refusal("Codex profile ancestry is too complex"));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn protected_ancestors(_path: &Path) -> io::Result<()> {
    Err(refusal(
        "Codex profile ancestry inspection is unsupported on this platform",
    ))
}

fn ancestors(cwd: &Path) -> io::Result<BTreeSet<PathBuf>> {
    normalized_absolute(cwd)?;
    let canonical = cwd
        .canonicalize()
        .map_err(|_| refusal("Codex workspace cannot be resolved"))?;
    let paths = cwd
        .ancestors()
        .chain(canonical.ancestors())
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>();
    if paths.len() > 256 {
        return Err(refusal("Codex workspace ancestry is too deep"));
    }
    Ok(paths)
}
fn quoted(path: &Path) -> io::Result<String> {
    path.to_str()
        .and_then(|path| serde_json::to_string(path).ok())
        .ok_or_else(|| refusal("Codex profile path is not supported"))
}
fn set_env(env: &mut Vec<EnvVar>, name: &str, value: OsString) {
    env.retain(|var| var.name() != name);
    env.push(EnvVar::new(name, value));
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use dudes_providers::adapter::TurnSession;
    use dudes_types::ModelSelection;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Probe {
        calls: Mutex<Vec<LaunchSpec>>,
        bad_metadata: bool,
        version: Option<&'static str>,
    }
    impl PredicateRunner for Probe {
        fn run(&self, spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
            self.calls.lock().unwrap().push(spec.clone());
            let output = if spec
                .argv
                .first()
                .is_some_and(|program| program == "/usr/bin/osascript")
            {
                if self.bad_metadata {
                    "{\"synchronized\":false,\"config\":false,\"requirements\":false}"
                } else {
                    "{\"synchronized\":true,\"config\":false,\"requirements\":false}"
                }
            } else {
                self.version.unwrap_or("codex-cli 0.160.0\n")
            };
            Ok(PredicateOutcome::Finished {
                exit: Some(0),
                output_tail: output.into(),
                timed_out: false,
            })
        }
        fn sweep(&self) {}
    }
    fn startup(root: &Path, cwd: PathBuf) -> CodexStartup {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("state/agents/a/outbox"))
            .unwrap();
        CodexStartup::new(
            root.join("state/providers/codex"),
            root.join("state/agents/a"),
            cwd,
            "test task".into(),
            TurnSession::Assigned,
            ModelSelection::provider_default(),
        )
    }
    fn spec(cwd: &Path) -> LaunchSpec {
        LaunchSpec {
            argv: vec!["/fake/codex".into(), "app-server".into()],
            cwd: cwd.to_owned(),
            env: vec![
                EnvVar::new("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "bad"),
                EnvVar::new("CODEX_SQLITE_HOME", "/unsafe"),
                EnvVar::new("TMPDIR", "/unsafe"),
            ],
            stdin: dudes_providers::launch::LaunchInput::Null,
        }
    }
    #[test]
    fn managed_profile_has_no_config_and_all_alias_ancestors_are_untrusted() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("worktrees/task/nested");
        std::fs::create_dir_all(&real).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let probe = Arc::new(Probe::default());
        let profile = CodexProfile::new(&root.path().join("state"), &[], probe.clone());
        let mut launch = spec(&alias);
        let prepared = profile
            .prepare(&mut launch, &startup(root.path(), alias.clone()))
            .unwrap();
        assert!(!prepared.home().join("config.toml").exists());
        let project_arg = launch
            .argv
            .iter()
            .find_map(|arg| arg.to_str().filter(|arg| arg.starts_with("projects=")))
            .unwrap();
        for path in ancestors(&alias).unwrap() {
            assert!(project_arg.contains(&format!(
                "{} = {{ trust_level = \"untrusted\" }}",
                quoted(&path).unwrap()
            )));
        }
        assert!(
            !launch
                .env
                .iter()
                .any(|var| var.name() == "CODEX_SQLITE_HOME"
                    || var.name() == "CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        );
        assert!(
            launch.env.iter().any(|var| var.name() == "TMPDIR"
                && var.expose_value() == prepared.agent_dir().join("tmp"))
        );
        profile.before_task(&prepared).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(
            probe
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|spec| spec.argv[0] == "/usr/bin/osascript")
                .count(),
            2
        );
    }
    #[test]
    fn profile_rejects_configuration_symlinks_overlap_and_changed_cwd_target() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let profile =
            CodexProfile::new(&root.path().join("state"), &[], Arc::new(Probe::default()));
        let prepared = profile
            .prepare(&mut spec(&cwd), &startup(root.path(), cwd.clone()))
            .unwrap();
        std::fs::write(
            prepared.home().join("config.toml"),
            "secret canary never read",
        )
        .unwrap();
        assert!(profile.before_task(&prepared).is_err());
        std::fs::remove_file(prepared.home().join("config.toml")).unwrap();
        assert!(
            profile
                .prepare(
                    &mut spec(root.path()),
                    &startup(root.path(), root.path().to_owned())
                )
                .is_err()
        );
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&cwd, &alias).unwrap();
        let prepared = profile
            .prepare(&mut spec(&alias), &startup(root.path(), alias.clone()))
            .unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        assert!(profile.before_task(&prepared).is_err());
        std::os::unix::fs::symlink(root.path().join("missing"), prepared.home().join("rules"))
            .unwrap();
        assert!(matches!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Unavailable { .. }
        ));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn unavailable_metadata_refuses_login_without_falling_back_to_user_home() {
        let root = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            bad_metadata: true,
            ..Probe::default()
        });
        let profile = CodexProfile::new(
            root.path(),
            &[
                EnvVar::new("CODEX_HOME", "/operator/home"),
                EnvVar::new("DUDES_TOKEN", "not-a-real-token"),
            ],
            probe.clone(),
        );
        assert!(matches!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Unavailable { .. }
        ));
        let calls = probe.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].env.iter().all(|var| var.name() != "DUDES_TOKEN"));
    }
    fn reporting(version: &'static str) -> Arc<Probe> {
        Arc::new(Probe {
            version: Some(version),
            ..Probe::default()
        })
    }
    #[test]
    fn a_patch_release_of_the_supported_line_is_admitted() {
        let root = tempfile::tempdir().unwrap();
        let profile = CodexProfile::new(root.path(), &[], reporting("codex-cli 0.160.1\n"));
        assert_eq!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Ready {
                path: root.path().join("providers/codex").canonicalize().unwrap()
            }
        );
    }
    #[test]
    fn another_minor_is_unavailable_before_any_protocol_launch_and_names_both_versions() {
        let root = tempfile::tempdir().unwrap();
        let profile = CodexProfile::new(root.path(), &[], reporting("codex-cli 0.161.0\n"));
        assert_eq!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Unavailable {
                reason: "Codex streaming requires codex-cli 0.160.x or 0.164.x, but the installed Codex reports codex-cli 0.161.0".into()
            }
        );
    }
    #[test]
    fn writable_profile_parents_are_refused_without_changing_their_modes() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let root = tempfile::tempdir().unwrap();
        let profile =
            CodexProfile::new(&root.path().join("state"), &[], Arc::new(Probe::default()));
        assert!(matches!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Ready { .. }
        ));
        for (path, mode) in [
            (root.path().join("state/providers"), 0o770),
            (root.path().join("state"), 0o777),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                profile.login_home(Some(Path::new("/fake/codex"))),
                ProviderLoginHome::Unavailable { .. }
            ));
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, mode);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn protected_aliases_and_sticky_temp_roots_are_supported_but_unsafe_targets_are_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let profile = CodexProfile::new(&alias.join("state"), &[], Arc::new(Probe::default()));
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(matches!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Ready { .. }
        ));
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            profile.login_home(Some(Path::new("/fake/codex"))),
            ProviderLoginHome::Unavailable { .. }
        ));
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
