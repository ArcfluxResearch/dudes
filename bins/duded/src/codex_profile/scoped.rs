use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use dudes_proto::instruction::InstructionObservationOwner;
use dudes_providers::codex::startup::CodexStartup;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_providers::scoped::{ScopedManagedLaunch, ScopedPlatform};
use dudes_types::{Lsn, TurnId};
use sha2::{Digest as _, Sha256};

use super::{CodexProfile, PROBE_DEADLINE, absent, owner_directory, quoted, refusal};
use crate::predicate::PredicateCaptured;

mod inspection;
use inspection::Inspection;

const CATALOG_LIMIT: usize = 720_896;
const CATALOG_LENGTH: usize = 658_607;
const CATALOG_SHA: &str = "69af12f94a39dabd1a1ded8f5257b4884d2118902ea2b5b9cffbf4a1925dfa65";
const MACOS_BINARY_SHA: &str = "97026b7ea245a7a9f17d8522a2ba4a8cd2f1c01dac0f62483a4bfe427d76082e";

#[derive(Debug, Default)]
pub(crate) struct PrivateRoots(Mutex<u64>);

impl PrivateRoots {
    pub(crate) fn inspect(&self) -> io::Result<MutexGuard<'_, u64>> {
        self.0
            .try_lock()
            .map_err(|_| refusal("private root inspection was interrupted"))
    }

    pub(crate) fn mutate(&self) -> io::Result<MutexGuard<'_, u64>> {
        let mut generation = self.inspect()?;
        *generation = generation
            .checked_add(1)
            .ok_or_else(|| refusal("private root generation is exhausted"))?;
        Ok(generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub(crate) turn: TurnId,
    pub(crate) generation: u64,
    pub(crate) started: Lsn,
    pub(crate) owner: InstructionObservationOwner,
}

pub(crate) struct PreparedScopedLaunch {
    pub(crate) launch: ScopedManagedLaunch,
    pub(crate) attempt: Attempt,
    pub(crate) binding: Arc<ScopedBinding>,
}

pub(crate) struct ScopedBinding {
    profile: Arc<CodexProfile>,
    generation: u64,
    startup: CodexStartup,
    inspection: Inspection,
    binary: PinnedFile,
    catalog: PinnedFile,
}

impl std::fmt::Debug for ScopedBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedBinding")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl ScopedBinding {
    pub(crate) fn before_task(&self, startup: &CodexStartup) -> io::Result<()> {
        let generation = self.profile.mutations.inspect()?;
        if *generation != self.generation || startup != &self.startup {
            return Err(refusal("scoped launch preparation changed during startup"));
        }
        self.profile.scoped_policy(false)?;
        let deadline = Instant::now() + PROBE_DEADLINE;
        self.binary.check_identity()?;
        self.catalog.check(deadline)?;
        self.inspection.check_roots()
    }
}

impl CodexProfile {
    pub(crate) fn private_roots(&self) -> Arc<PrivateRoots> {
        Arc::clone(&self.mutations)
    }

    pub(crate) fn scoped_home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn prepare_scoped(
        self: &Arc<Self>,
        mut launch: ScopedManagedLaunch,
        attempt: Attempt,
    ) -> io::Result<PreparedScopedLaunch> {
        validate_environment(&launch)?;
        if attempt.started == Lsn::ZERO
            || attempt.generation == 0
            || !matches!(attempt.owner, InstructionObservationOwner::Managed { turn, .. } if turn == attempt.turn)
        {
            return Err(refusal("scoped physical attempt identity is unavailable"));
        }
        let generation = self.mutations.mutate()?;
        self.scoped_policy(true)?;
        let deadline = Instant::now() + PROBE_DEADLINE;
        #[cfg(not(test))]
        let qualification = Qualification::current(launch.inventory.platform)?;
        #[cfg(test)]
        let qualification = self.scoped_test_qualification.as_ref().map_or_else(
            || Qualification::current(launch.inventory.platform),
            |(binary_sha, catalog_sha, catalog_len)| {
                Ok(Qualification {
                    binary_sha: binary_sha.clone(),
                    catalog_sha: catalog_sha.clone(),
                    catalog_len: *catalog_len,
                })
            },
        )?;
        let program = launch
            .spec
            .argv
            .first()
            .map(Path::new)
            .ok_or_else(|| refusal("scoped executable is unavailable"))?;
        let binary = PinnedFile::open(program, &qualification.binary_sha, None, deadline)?;
        self.install_scoped(&launch)?;
        let catalog = self.bundled_catalog(&binary, &qualification, deadline)?;
        Self::configure_scoped(&mut launch, &catalog.path)?;
        let inspection = Inspection::capture(&launch.inventory, deadline)?;
        let LaunchInput::Protocol(startup) = &launch.spec.stdin else {
            return Err(refusal("scoped Codex startup is unavailable"));
        };
        let binding = Arc::new(ScopedBinding {
            profile: Arc::clone(self),
            generation: *generation,
            startup: startup.clone(),
            inspection,
            binary,
            catalog,
        });
        Ok(PreparedScopedLaunch {
            launch,
            attempt,
            binding,
        })
    }

    fn scoped_policy(&self, create: bool) -> io::Result<()> {
        absent(&self.home.join(".env"))?;
        #[cfg(unix)]
        absent(Path::new("/etc/codex/config.toml"))?;
        self.check_home(create)?;
        self.machine_policy()
    }

    fn install_scoped(&self, launch: &ScopedManagedLaunch) -> io::Result<()> {
        let p = &launch.inventory.private;
        if p.profile != self.home
            || !launch.install.files.is_empty()
            || launch.install.directories
                != [
                    p.home.clone(),
                    p.profile.clone(),
                    p.work.clone(),
                    p.outbox.clone(),
                    p.temp.clone(),
                ]
        {
            return Err(refusal("scoped install does not belong to this profile"));
        }
        for directory in &launch.install.directories {
            owner_directory(directory)?;
        }
        Ok(())
    }

    fn configure_scoped(launch: &mut ScopedManagedLaunch, catalog: &Path) -> io::Result<()> {
        let inventory = &launch.inventory;
        let LaunchInput::Protocol(startup) = &launch.spec.stdin else {
            return Err(refusal("scoped Codex startup is unavailable"));
        };
        if startup.home() != inventory.private.profile
            || startup.cwd() != inventory.cwd
            || launch.spec.cwd != inventory.cwd
            || startup.scoped_roots() != Some(inventory.additional_roots().as_slice())
        {
            return Err(refusal("scoped startup differs from its frozen roots"));
        }
        let roots = serde_json::to_string(&inventory.additional_roots())
            .map_err(|_| refusal("scoped roots cannot be encoded"))?;
        let mut ancestors = BTreeSet::new();
        for path in inventory
            .writable_roots()
            .iter()
            .chain(&inventory.protected)
        {
            ancestors.extend(super::ancestors(path.parent().unwrap_or(path))?);
            ancestors.extend(super::ancestors(path)?);
        }
        let projects = ancestors
            .iter()
            .map(|path| quoted(path).map(|path| format!("{path}={{trust_level=\"untrusted\"}}")))
            .collect::<io::Result<Vec<_>>>()?
            .join(",");
        let mut options = std::collections::BTreeMap::from([
            ("features.multi_agent", "false".to_owned()),
            ("features.goals", "false".to_owned()),
            ("web_search", "\"disabled\"".to_owned()),
            ("mcp_servers", "{}".to_owned()),
            ("projects", format!("{{{projects}}}")),
            ("sqlite_home", quoted(&inventory.private.profile)?),
            ("log_dir", quoted(&inventory.private.profile.join("log"))?),
            ("sandbox_workspace_write.writable_roots", roots),
        ]);
        let program = launch
            .spec
            .argv
            .first()
            .ok_or_else(|| refusal("scoped executable is unavailable"))?;
        let prefix = dudes_providers::codex::model_list::argv(program.clone());
        if !launch.spec.argv.starts_with(&prefix) {
            return Err(refusal("scoped native startup arguments changed"));
        }
        let mut index = prefix.len();
        while index < launch.spec.argv.len() {
            if launch.spec.argv[index] != "-c" {
                return Err(refusal("scoped native startup has an unowned argument"));
            }
            let setting = launch
                .spec
                .argv
                .get_mut(index + 1)
                .ok_or_else(|| refusal("scoped configuration is incomplete"))?;
            let key = setting
                .to_str()
                .and_then(|setting| setting.split_once('='))
                .map(|(key, _)| key.to_owned())
                .ok_or_else(|| refusal("scoped configuration is invalid"))?;
            let value = options
                .remove(key.as_str())
                .ok_or_else(|| refusal("scoped configuration has an unowned override"))?;
            *setting = format!("{key}={value}").into();
            index += 2;
        }
        if !options.is_empty() {
            return Err(refusal("scoped configuration is incomplete"));
        }
        launch.spec.argv.extend([
            "-c".into(),
            format!("model_catalog_json={}", quoted(catalog)?).into(),
        ]);
        launch.spec.stdin = LaunchInput::Protocol(startup.resolved_paths(
            inventory.private.profile.clone(),
            startup.agent_dir().to_owned(),
            inventory.cwd.clone(),
        ));
        Ok(())
    }

    fn bundled_catalog(
        &self,
        binary: &PinnedFile,
        qualification: &Qualification,
        deadline: Instant,
    ) -> io::Result<PinnedFile> {
        let directory = self.home.join("catalogs");
        owner_directory(&directory)?;
        let path = directory.join(format!("{}.json", qualification.catalog_sha));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return PinnedFile::open(
                    &path,
                    &qualification.catalog_sha,
                    Some(qualification.catalog_len),
                    deadline,
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| refusal("scoped export scratch identity is unavailable"))?;
        let scratch = directory.join(format!("export-{}", ulid::Ulid::from_bytes(nonce)));
        owner_directory(&scratch)?;
        let result = (|| {
            for name in ["home", "profile", "tmp", "work"] {
                owner_directory(&scratch.join(name))?;
            }
            let spec = offline_export(binary, &scratch)?;
            let bytes = match self
                .runner
                .run_captured(&spec, PROBE_DEADLINE, CATALOG_LIMIT)?
            {
                PredicateCaptured::Finished {
                    exit: Some(0),
                    stdout,
                    truncated: false,
                    timed_out: false,
                } if Instant::now() < deadline => stdout,
                _ => return Err(refusal("scoped bundled catalog export is incomplete")),
            };
            validate_catalog(&bytes, qualification)?;
            binary.check(deadline)?;
            let staged = scratch.join("catalog.json");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
            }
            std::fs::hard_link(&staged, &path)?;
            std::fs::remove_file(&staged)?;
            PinnedFile::open(
                &path,
                &qualification.catalog_sha,
                Some(qualification.catalog_len),
                deadline,
            )
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }
}

fn validate_environment(launch: &ScopedManagedLaunch) -> io::Result<()> {
    use std::ffi::OsStr;
    let p = &launch.inventory.private;
    let mut expected = std::collections::BTreeMap::from([
        ("HOME", p.home.as_os_str()),
        ("CODEX_HOME", p.profile.as_os_str()),
        ("TMPDIR", p.temp.as_os_str()),
        ("TMP", p.temp.as_os_str()),
        ("TEMP", p.temp.as_os_str()),
        ("SHELL", OsStr::new("/bin/bash")),
        ("TERM", OsStr::new("dumb")),
    ]);
    let mut seen = BTreeSet::new();
    for var in &launch.spec.env {
        let name = var
            .name()
            .to_str()
            .ok_or_else(|| refusal("scoped environment name is invalid"))?;
        if !seen.insert(name) {
            return Err(refusal("scoped environment is ambiguous"));
        }
        if let Some(value) = expected.remove(name) {
            if var.expose_value() != value {
                return Err(refusal("scoped private environment changed"));
            }
        } else if !matches!(name, "PATH" | "LANG" | "OPENAI_API_KEY" | "CODEX_API_KEY") {
            return Err(refusal("scoped environment contains an unowned setting"));
        }
    }
    if !expected.is_empty() {
        return Err(refusal("scoped private environment is incomplete"));
    }
    Ok(())
}

struct Qualification {
    binary_sha: String,
    catalog_sha: String,
    catalog_len: usize,
}

impl Qualification {
    fn current(platform: ScopedPlatform) -> io::Result<Self> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || platform != ScopedPlatform::MacOsArm64
        {
            return Err(refusal(
                "scoped bundled catalog has no qualified platform export",
            ));
        }
        Ok(Self {
            binary_sha: MACOS_BINARY_SHA.into(),
            catalog_sha: CATALOG_SHA.into(),
            catalog_len: CATALOG_LENGTH,
        })
    }
}

fn offline_export(binary: &PinnedFile, scratch: &Path) -> io::Result<LaunchSpec> {
    let policy = format!(
        "(version 1)(allow default)(deny network*)(deny file-write*)(allow file-write* (subpath {}))(deny file-read* (subpath \"/Users\"))(deny file-read* (subpath \"/private/var/root\"))(allow file-read* (subpath {}))(allow file-read* (literal {}))",
        quoted(scratch)?,
        quoted(scratch)?,
        quoted(&binary.path)?
    );
    let mut env = vec![
        EnvVar::new("HOME", scratch.join("home")),
        EnvVar::new("CODEX_HOME", scratch.join("profile")),
        EnvVar::new("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
        EnvVar::new("SHELL", "/bin/bash"),
        EnvVar::new("TERM", "dumb"),
        EnvVar::new("LANG", "C"),
        EnvVar::new("LC_ALL", "C"),
    ];
    for name in ["TMPDIR", "TMP", "TEMP"] {
        env.push(EnvVar::new(name, scratch.join("tmp")));
    }
    Ok(LaunchSpec {
        argv: vec![
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            policy.into(),
            binary.path.as_os_str().to_owned(),
            "debug".into(),
            "models".into(),
            "--bundled".into(),
        ],
        env,
        cwd: scratch.join("work"),
        stdin: LaunchInput::Null,
    })
}

fn validate_catalog(bytes: &[u8], qualification: &Qualification) -> io::Result<()> {
    if bytes.len() != qualification.catalog_len || digest(bytes) != qualification.catalog_sha {
        return Err(refusal(
            "scoped bundled catalog does not match the pinned executable",
        ));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| refusal("scoped bundled catalog is invalid"))?;
    let models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .filter(|models| !models.is_empty())
        .ok_or_else(|| refusal("scoped bundled catalog has no models"))?;
    let mut selected = models.iter().filter(|model| model["slug"] == "gpt-5.5");
    let model = selected
        .next()
        .ok_or_else(|| refusal("scoped model is absent from the bundled catalog"))?;
    if selected.next().is_some()
        || model["shell_type"] != "unified_exec"
        || model["apply_patch_tool_type"] != "freeform"
        || model["node_repl_disabled"] != false
        || model["node_repl_auto_review_required"] != false
        || model["experimental_supported_tools"] != serde_json::json!([])
        || !model["tool_mode"].is_null()
        || model["supports_search_tool"] != true
        || model["web_search_tool_type"] != "text_and_image"
    {
        return Err(refusal(
            "scoped model tool metadata differs from the pinned catalog",
        ));
    }
    Ok(())
}

struct PinnedFile {
    path: PathBuf,
    file: File,
    sha: String,
    len: u64,
    modified: std::time::SystemTime,
    changed: (i64, i64),
}

impl PinnedFile {
    fn open(path: &Path, sha: &str, len: Option<usize>, deadline: Instant) -> io::Result<Self> {
        let file = inspection::open_file(path)?;
        let metadata = file.metadata()?;
        let pinned = Self {
            path: path.to_owned(),
            file,
            sha: sha.to_owned(),
            len: metadata.len(),
            modified: metadata.modified()?,
            changed: inspection::changed(&metadata),
        };
        if len.is_some_and(|len| u64::try_from(len).ok() != Some(pinned.len)) {
            return Err(refusal("scoped pinned file length changed"));
        }
        pinned.check(deadline)?;
        Ok(pinned)
    }

    fn check(&self, deadline: Instant) -> io::Result<()> {
        self.check_identity()?;
        let mut file = inspection::open_file(&self.path)?;
        let metadata = file.metadata()?;
        if !inspection::same(&metadata, &self.file.metadata()?) || metadata.len() != self.len {
            return Err(refusal("scoped pinned file identity changed"));
        }
        file.rewind()?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            if Instant::now() >= deadline {
                return Err(refusal("scoped file inspection deadline expired"));
            }
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        if crate::enrollment_reload::hex(&hash.finalize()) != self.sha
            || !inspection::same(&metadata, &std::fs::symlink_metadata(&self.path)?)
        {
            return Err(refusal("scoped pinned file bytes changed"));
        }
        Ok(())
    }

    fn check_identity(&self) -> io::Result<()> {
        let file = inspection::open_file(&self.path)?;
        let metadata = file.metadata()?;
        if !inspection::same(&metadata, &self.file.metadata()?)
            || metadata.len() != self.len
            || metadata.modified()? != self.modified
            || inspection::changed(&metadata) != self.changed
        {
            return Err(refusal("scoped pinned file metadata changed"));
        }
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    crate::enrollment_reload::hex(&Sha256::digest(bytes))
}

#[cfg(all(test, unix))]
mod tests;
