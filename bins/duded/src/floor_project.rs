//! The Floor's add and remove of a project: the shapes of its requests and answers, and the rule
//! for which folders it will take.
//!
//! A newcomer types a folder, the page previews it, and one click adds it to the operator's file
//! through the same edit `dudes project add` runs. Removing one runs the edit `dudes project
//! remove` runs. The daemon then adopts the file at once. The
//! folder is canonicalized here, the repository's suggested check is computed again on every
//! add, and no git command, hook, build or check runs while a folder is validated.

use std::fs;
use std::path::{Component, Path, PathBuf};

use dudes_config::check::changed_check;
use dudes_proto::record::Hex32;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

/// Most bytes one project request body may carry.
pub const MAX_PROJECT_BODY_BYTES: usize = 4096;

/// Most bytes in a typed folder.
const MAX_FOLDER_BYTES: usize = 1024;

/// Most bytes in a project id a remove names.
const MAX_PROJECT_ID_BYTES: usize = 128;

/// Most words, and most bytes in each, of a check a request sends back.
const MAX_CHECK_WORDS: usize = 16;
const MAX_CHECK_WORD_BYTES: usize = 256;

/// The check's name on the wire when it is the default "files changed" check.
const FILES_CHANGED: &str = "files_changed";

/// Why a folder or an add was refused. Nothing was written in any case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The folder does not exist or cannot be read.
    NotFound,
    /// The path is not a folder.
    NotAFolder,
    /// The folder is one Dudes will not take: the filesystem root, the home folder, a hidden
    /// folder under home, `~/Library`, or one of Dudes' own folders.
    NotAllowed,
    /// The folder has no `.git` directory.
    NotGit,
    /// The folder or its suggested check is not what the preview showed.
    Changed,
    /// Another add or reload is running.
    Busy,
    /// The daemon could not adopt the file. It was put back as it was.
    ConfigChanged,
    /// The daemon already runs as many projects as it allows.
    Full,
    /// The project to remove is not one the daemon runs.
    Unknown,
    /// An agent that is not retired works in the project.
    Working,
    /// A ticket that is not done names the project.
    OpenTickets,
    /// The new project's name is not one Dudes will create.
    BadName,
    /// No folder is set for new projects, or this platform cannot create one.
    NoParent,
    /// The folder for new projects cannot be used as it is spelled.
    ParentUnusable,
    /// Something already has the new project's name.
    Exists,
    /// There is no git new enough to start a repository.
    GitUnavailable,
    /// Starting the repository failed after the folder was made.
    CreateFailed,
}

impl Reason {
    /// The fixed code the page shows.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotAFolder => "not_a_folder",
            Self::NotAllowed => "not_allowed",
            Self::NotGit => "not_git",
            Self::Changed => "changed",
            Self::Busy => "busy",
            Self::ConfigChanged => "config_changed",
            Self::Full => "full",
            Self::Unknown => "unknown",
            Self::Working => "working",
            Self::OpenTickets => "open_tickets",
            Self::BadName => "bad_name",
            Self::NoParent => "no_parent",
            Self::ParentUnusable => "parent_unusable",
            Self::Exists => "exists",
            Self::GitUnavailable => "git_unavailable",
            Self::CreateFailed => "create_failed",
        }
    }
}

/// Why a project request got no answer of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A reason the page explains.
    Reason(Reason),
    /// The switch went off after the request was queued.
    ActionOff,
    /// The session expired or was revoked.
    Forbidden,
    /// The daemon is ending.
    Unavailable,
}

impl From<Reason> for Refusal {
    fn from(reason: Reason) -> Self {
        Self::Reason(reason)
    }
}

/// A completion check on the wire: the words `files_changed`, or the command's argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The default check: the agent has edited or added a file.
    FilesChanged,
    /// A repository's own test command.
    Command(Vec<String>),
}

impl Check {
    /// The check for `argv`.
    #[must_use]
    pub fn of(argv: Vec<String>) -> Self {
        if argv == changed_check() {
            Self::FilesChanged
        } else {
            Self::Command(argv)
        }
    }

    /// The command the daemon runs.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::FilesChanged => changed_check(),
            Self::Command(argv) => argv.clone(),
        }
    }
}

impl Serialize for Check {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::FilesChanged => serializer.serialize_str(FILES_CHANGED),
            Self::Command(argv) => argv.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Check {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Name(String),
            Argv(Vec<String>),
        }
        let argv = match Wire::deserialize(deserializer)? {
            Wire::Name(name) if name == FILES_CHANGED => return Ok(Self::FilesChanged),
            Wire::Name(_) => return Err(serde::de::Error::custom("unknown check")),
            Wire::Argv(argv) => argv,
        };
        if argv.is_empty()
            || argv.len() > MAX_CHECK_WORDS
            || argv.iter().any(|word| word.len() > MAX_CHECK_WORD_BYTES)
        {
            return Err(serde::de::Error::custom("a check of unreasonable size"));
        }
        Ok(Self::of(argv))
    }
}

/// `POST /floor/project/preview`: the folder to look at.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub(crate) path: String,
}

/// `POST /floor/project/add` as the page sends it: the canonical folder and check the preview
/// showed, and the identity of this attempt.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    folder: String,
    check: Check,
    #[serde(default)]
    label: Option<String>,
    attempt_id: String,
}

/// `POST /floor/project/remove` as the page sends it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveBody {
    project_id: String,
    attempt_id: String,
}

/// One bounded remove request.
#[derive(Clone, Debug)]
pub struct RemoveRequest {
    pub(crate) attempt_id: Hex32,
    pub(crate) digest: Hex32,
    pub(crate) project_id: String,
}

/// One bounded add request.
#[derive(Clone, Debug)]
pub struct AddRequest {
    pub(crate) attempt_id: Hex32,
    pub(crate) attempt_wire: String,
    pub(crate) digest: Hex32,
    pub(crate) folder: String,
    pub(crate) check: Check,
    pub(crate) label: Option<String>,
}

/// `POST /floor/project/create` as the page sends it. The folder is never the page's to name: only
/// a child name, beneath the parent the operator's file sets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    #[serde(default)]
    label: Option<String>,
    attempt_id: String,
}

/// A name for a new project's folder: 1 to 64 characters, starting with a letter or digit, then
/// letters, digits, `.`, `_` or `-`. It cannot be a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildName(String);

impl ChildName {
    /// The name `text` spells, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = text.bytes();
        let first = bytes.next()?;
        (text.len() <= 64
            && first.is_ascii_alphanumeric()
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
        .then(|| Self(text.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded create request.
#[derive(Clone, Debug)]
#[cfg_attr(not(unix), allow(dead_code, reason = "only a Unix creation reads it"))]
pub struct CreateRequest {
    pub(crate) attempt_id: Hex32,
    pub(crate) attempt_wire: String,
    pub(crate) digest: Hex32,
    pub(crate) name: ChildName,
    pub(crate) label: Option<String>,
}

/// Why a create body is not a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateParse {
    /// Wrong shape, too large, an unknown field such as a path, or a bad attempt.
    Malformed,
    /// The name is not one Dudes will create.
    BadName,
}

/// Where the daemon stands before it makes the folder, read on the loop.
#[derive(Clone, Debug)]
#[cfg_attr(not(unix), allow(dead_code, reason = "only a Unix creation reads it"))]
pub struct CreatePlan {
    /// The parent as the operator's file spells it, `~/` expanded.
    pub(crate) parent: PathBuf,
    /// The SHA-256 of the operator's file when the plan was made.
    pub(crate) config_sha: Hex32,
}

/// What the loop is asked to publish once the folder and its repository exist.
#[derive(Clone, Debug)]
pub struct Enroll {
    pub(crate) folder: PathBuf,
    pub(crate) label: String,
    pub(crate) config_sha: Hex32,
    /// The sequence number the caller took with the reload lock it still holds.
    pub(crate) seq: u64,
}

/// What became of the operator's file when an enrollment failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigState {
    /// It was never replaced, or is as it was.
    Unchanged,
    /// It held our bytes and was put back.
    Restored,
    /// Someone else changed it, so it was left alone.
    Conflict,
    /// It held our bytes and could not be put back.
    Failed,
    /// The daemon did not answer, or did not report the project once the file held it, so what
    /// the file holds is not known to the caller.
    Unknown,
}

impl ConfigState {
    /// The fixed code the page shows.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Restored => "restored",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// An enrollment that did not happen. `replaced` is true once the file may hold the new project:
/// from then on the folder is never removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnrollFailure {
    /// Why it failed.
    pub reason: Reason,
    /// Whether the file may hold the new project.
    pub replaced: bool,
    /// What the file holds now.
    pub config: ConfigState,
}

/// What a project job carries.
#[derive(Clone, Debug)]
pub enum ProjectRequest {
    /// Read what creating a project needs from the loop.
    CreatePlan,
    /// Enroll the folder just created.
    CreateEnroll(Enroll),
    /// Look at a folder and say what adding it would do.
    Preview(PreviewRequest),
    /// Add the folder.
    Add(AddRequest),
    /// Take a project out of the file and the running daemon.
    Remove(RemoveRequest),
}

/// A project that is, or would be, in the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectRef {
    /// Its id.
    pub project_id: String,
    /// Its label.
    pub label: String,
}

/// What a preview says about a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Preview {
    /// The folder as Dudes would keep it, symlinks resolved.
    pub folder: String,
    /// The project already at this folder, if any.
    pub already: Option<ProjectRef>,
    /// The check Dudes would run.
    pub check: Check,
    /// The check in the words the page shows.
    pub check_label: String,
    /// Whether a hire can use the project as soon as it is added.
    pub hireable: bool,
    /// Why not: `ceiling_required` or `check_not_runnable`.
    pub reason: Option<&'static str>,
}

/// What an add did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Added {
    /// The project's id.
    pub project_id: String,
    /// The project's label.
    pub label: String,
    /// The check in the words the page shows.
    pub check_label: String,
    /// Whether a hire can use the project now.
    pub hireable: bool,
    /// Why not: `ceiling_required` or `check_not_runnable`.
    pub reason: Option<&'static str>,
}

/// What a remove did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Removed {
    /// The id the project had.
    pub project_id: String,
    /// The label the project had.
    pub label: String,
}

/// The answer to a project job.
#[derive(Clone, Debug)]
pub enum ProjectAnswer {
    /// What creating a project needs.
    Plan(CreatePlan),
    /// A created folder's enrollment.
    Enrolled(Result<Added, EnrollFailure>),
    /// A preview.
    Preview(Preview),
    /// An add, new or repeated.
    Added(Added),
    /// A remove.
    Removed(Removed),
}

/// The preview request a body holds, or `None` when it is the wrong shape or too large.
pub(crate) fn parse_preview(bytes: &[u8]) -> Option<PreviewRequest> {
    let request: PreviewRequest = parse(bytes)?;
    (request.path.len() <= MAX_FOLDER_BYTES).then_some(request)
}

/// The add request a body holds, or `None` when it is the wrong shape, too large or names an
/// attempt that is not 32 bytes of base64url.
pub(crate) fn parse_add(bytes: &[u8]) -> Option<AddRequest> {
    let body: AddBody = parse(bytes)?;
    if body.folder.len() > MAX_FOLDER_BYTES
        || body.label.as_deref().is_some_and(|label| {
            label.is_empty() || label.len() > 128 || label.chars().any(char::is_control)
        })
    {
        return None;
    }
    let attempt_id = crate::floor_actions::decode_attempt(&body.attempt_id).ok()?;
    let digest = digest(&body.folder, &body.check, body.label.as_deref());
    Some(AddRequest {
        attempt_id,
        attempt_wire: body.attempt_id,
        digest,
        folder: body.folder,
        check: body.check,
        label: body.label,
    })
}

/// The create request a body holds.
pub(crate) fn parse_create(bytes: &[u8]) -> Result<CreateRequest, CreateParse> {
    let body: CreateBody = parse(bytes).ok_or(CreateParse::Malformed)?;
    if body.label.as_deref().is_some_and(|label| {
        label.is_empty() || label.len() > 128 || label.chars().any(char::is_control)
    }) {
        return Err(CreateParse::Malformed);
    }
    let attempt_id = crate::floor_actions::decode_attempt(&body.attempt_id)
        .map_err(|_| CreateParse::Malformed)?;
    let name = ChildName::parse(&body.name).ok_or(CreateParse::BadName)?;
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.project.create.v1\0");
    for part in [name.as_str(), body.label.as_deref().unwrap_or("")] {
        hasher.update(
            u32::try_from(part.len())
                .expect("bounded field")
                .to_be_bytes(),
        );
        hasher.update(part.as_bytes());
    }
    Ok(CreateRequest {
        attempt_id,
        attempt_wire: body.attempt_id,
        digest: Hex32::new(hasher.finalize().into()),
        name,
        label: body.label,
    })
}

/// The remove request a body holds, or `None` when it is the wrong shape, too large or names an
/// attempt that is not 32 bytes of base64url.
pub(crate) fn parse_remove(bytes: &[u8]) -> Option<RemoveRequest> {
    let body: RemoveBody = parse(bytes)?;
    if body.project_id.is_empty()
        || body.project_id.len() > MAX_PROJECT_ID_BYTES
        || body.project_id.chars().any(char::is_control)
    {
        return None;
    }
    let attempt_id = crate::floor_actions::decode_attempt(&body.attempt_id).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.project.remove.v1\0");
    hasher.update(body.project_id.as_bytes());
    Some(RemoveRequest {
        attempt_id,
        digest: Hex32::new(hasher.finalize().into()),
        project_id: body.project_id,
    })
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Option<T> {
    if bytes.len() > MAX_PROJECT_BODY_BYTES
        || bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{')
    {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// SHA-256 over what the attempt asks for, so a retry that asks for something else is a conflict.
fn digest(folder: &str, check: &Check, label: Option<&str>) -> Hex32 {
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.project.add.v1\0");
    let check = serde_json::to_vec(check).expect("a check is finite JSON");
    for part in [folder.as_bytes(), &check, label.unwrap_or("").as_bytes()] {
        hasher.update(
            u32::try_from(part.len())
                .expect("bounded field")
                .to_be_bytes(),
        );
        hasher.update(part);
    }
    Hex32::new(hasher.finalize().into())
}

/// `path` as it will resolve once it exists: its nearest existing ancestor made canonical, then
/// the rest, so a link in the ancestor (`/var`, `/tmp`) still matches.
fn resolved_ahead(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(canonical) = at.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |whole, part| whole.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The folders the daemon will not take, and where "home" is.
#[derive(Clone, Debug, Default)]
pub struct Folders {
    home: Option<PathBuf>,
    own: Vec<PathBuf>,
}

impl Folders {
    /// The rule for a daemon whose home is `home` and whose own state, runtime and config folders
    /// are `own`. Each is resolved, so a link to one is refused as well.
    #[must_use]
    pub fn new(home: Option<&Path>, own: &[&Path]) -> Self {
        Self {
            home: home.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf())),
            own: own.iter().map(|path| resolved_ahead(path)).collect(),
        }
    }

    /// The canonical folder `typed` names, or why Dudes will not take it. `~` or `~/` at the start
    /// means the daemon's home.
    ///
    /// # Errors
    ///
    /// The first [`Reason`] that applies: `not_found`, `not_a_folder`, `not_allowed`, `not_git`.
    pub fn resolve(&self, typed: &str) -> Result<PathBuf, Reason> {
        if self.home.is_none() {
            return Err(Reason::NotAllowed);
        }
        let path = match typed
            .strip_prefix("~/")
            .or_else(|| (typed == "~").then_some(""))
        {
            Some(rest) => self.home.as_ref().ok_or(Reason::NotFound)?.join(rest),
            None => PathBuf::from(typed),
        };
        if !path.is_absolute() {
            return Err(Reason::NotFound);
        }
        let metadata = fs::metadata(&path).map_err(|_| Reason::NotFound)?;
        if !metadata.is_dir() {
            return Err(Reason::NotAFolder);
        }
        let canonical = path.canonicalize().map_err(|_| Reason::NotFound)?;
        if !self.allows(&canonical) {
            return Err(Reason::NotAllowed);
        }
        fs::read_dir(&canonical).map_err(|_| Reason::NotFound)?;
        let git = fs::symlink_metadata(canonical.join(".git")).map_err(|_| Reason::NotGit)?;
        if !git.file_type().is_dir() {
            return Err(Reason::NotGit);
        }
        Ok(canonical)
    }

    /// The folder new projects are created in, from the operator's spelling of it. Only the spelling
    /// is judged here; whether it holds a symlink is judged when it is opened.
    ///
    /// # Errors
    ///
    /// [`Reason::ParentUnusable`] for a spelling that is not an absolute path made of plain names,
    /// or a folder Dudes will not take (the same rule as an added folder, without the `.git` need).
    pub fn create_parent(&self, spelled: &str) -> Result<PathBuf, Reason> {
        let path = match spelled.strip_prefix("~/") {
            Some(rest) => self.home.as_ref().ok_or(Reason::ParentUnusable)?.join(rest),
            None => PathBuf::from(spelled),
        };
        let plain = path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        (plain && self.allows(&path))
            .then_some(path)
            .ok_or(Reason::ParentUnusable)
    }

    fn allows(&self, folder: &Path) -> bool {
        if folder.parent().is_none() {
            return false;
        }
        if let Some(home) = &self.home {
            if folder == home {
                return false;
            }
            if let Ok(below) = folder.strip_prefix(home) {
                let mut names = below.components().filter_map(|part| match part {
                    Component::Normal(name) => name.to_str(),
                    _ => None,
                });
                if names.clone().any(|name| name.starts_with('.')) {
                    return false;
                }
                if cfg!(target_os = "macos")
                    && names
                        .next()
                        .is_some_and(|name| name.eq_ignore_ascii_case("Library"))
                {
                    return false;
                }
            }
        }
        !self
            .own
            .iter()
            .any(|own| folder.starts_with(own) || own.starts_with(folder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn git_repo(path: &Path) {
        fs::create_dir_all(path.join(".git")).expect("repository");
    }

    /// Every folder the rule refuses, and the one it takes, in one table.
    #[cfg(unix)]
    #[test]
    fn the_folder_rule_refuses_what_it_names() {
        let root = tempfile::tempdir().expect("root");
        let home = root.path().join("home");
        let state = home.join("state");
        let config = home.join("conf");
        for dir in [&home, &state, &config] {
            fs::create_dir_all(dir).expect("dir");
        }
        for repo in [
            home.join("app"),
            home.join(".hidden").join("app"),
            home.join("Library").join("app"),
            home.join("app").join("deep"),
            state.join("repo"),
            root.path().join("outside"),
        ] {
            git_repo(&repo);
        }
        git_repo(&home);
        git_repo(&state);
        fs::create_dir_all(home.join("plain")).expect("plain");
        fs::write(home.join("file"), "x").expect("file");
        let gitfile = home.join("worktree");
        fs::create_dir_all(&gitfile).expect("dir");
        fs::write(gitfile.join(".git"), "gitdir: elsewhere").expect("gitfile");
        std::os::unix::fs::symlink(state.join("repo"), home.join("link")).expect("link");
        std::os::unix::fs::symlink(home.join("app"), home.join("alias")).expect("alias");

        let folders = Folders::new(Some(&home), &[&state, &config]);
        let at = |path: &Path| folders.resolve(path.to_str().expect("utf8"));
        let library = if cfg!(target_os = "macos") {
            Err(Reason::NotAllowed)
        } else {
            Ok(home
                .join("Library")
                .join("app")
                .canonicalize()
                .expect("canonical"))
        };
        let canonical = |path: PathBuf| path.canonicalize().expect("canonical");

        assert_eq!(at(Path::new("/")), Err(Reason::NotAllowed), "root");
        assert_eq!(at(&home), Err(Reason::NotAllowed), "home");
        assert_eq!(
            at(&home.join(".hidden").join("app")),
            Err(Reason::NotAllowed),
            "dot"
        );
        assert_eq!(at(&home.join("Library").join("app")), library, "Library");
        assert_eq!(at(&state), Err(Reason::NotAllowed), "state folder");
        assert_eq!(
            at(&state.join("repo")),
            Err(Reason::NotAllowed),
            "inside state"
        );
        assert_eq!(at(&config), Err(Reason::NotAllowed), "config folder");
        assert_eq!(
            at(&home.join("link")),
            Err(Reason::NotAllowed),
            "link into state"
        );
        assert_eq!(at(&home.join("plain")), Err(Reason::NotGit), "no .git");
        assert_eq!(at(&gitfile), Err(Reason::NotGit), ".git a file");
        assert_eq!(at(&home.join("file")), Err(Reason::NotAFolder), "a file");
        assert_eq!(at(&home.join("missing")), Err(Reason::NotFound), "missing");
        assert_eq!(
            at(Path::new("relative/app")),
            Err(Reason::NotFound),
            "relative"
        );
        assert_eq!(
            at(&home.join("alias")),
            Ok(canonical(home.join("app"))),
            "link resolved"
        );
        assert_eq!(
            folders.resolve("~"),
            Err(Reason::NotAllowed),
            "home by its tilde"
        );
        assert_eq!(
            folders.resolve("~/app"),
            Ok(canonical(home.join("app"))),
            "tilde"
        );
        assert_eq!(
            at(&root.path().join("outside")),
            Ok(canonical(root.path().join("outside"))),
            "outside home"
        );
    }

    #[test]
    fn a_child_name_is_one_plain_name() {
        for good in ["a", "my-app", "app.v2", "A_b-c.d", &"x".repeat(64)] {
            assert_eq!(
                ChildName::parse(good).as_ref().map(ChildName::as_str),
                Some(good)
            );
        }
        for bad in [
            "",
            ".",
            "..",
            ".git",
            "-x",
            "_x",
            "a/b",
            "a\\b",
            "a b",
            "a\0b",
            "\u{e9}",
            "a\n",
            &"x".repeat(65),
        ] {
            assert_eq!(ChildName::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_creation_body_names_only_a_child_an_attempt_and_a_label() {
        let attempt = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let create = |body: serde_json::Value| parse_create(body.to_string().as_bytes());
        let ok = create(serde_json::json!({"name": "app", "attempt_id": attempt}))
            .expect("a name and an attempt");
        assert_eq!(ok.name.as_str(), "app");
        let labelled =
            create(serde_json::json!({"name": "app", "label": "App", "attempt_id": attempt}))
                .expect("a label");
        assert_ne!(
            ok.digest, labelled.digest,
            "the label is part of the request"
        );
        assert!(matches!(
            create(serde_json::json!({"name": "a/b", "attempt_id": attempt})),
            Err(CreateParse::BadName)
        ));
        for bad in [
            serde_json::json!({"name": "app", "attempt_id": attempt, "path": "/tmp/x"}),
            serde_json::json!({"name": "app", "attempt_id": attempt, "parent": "/tmp"}),
            serde_json::json!({"name": "app", "attempt_id": "short"}),
            serde_json::json!({"name": "app", "label": "", "attempt_id": attempt}),
            serde_json::json!({"attempt_id": attempt}),
        ] {
            assert!(
                matches!(create(bad.clone()), Err(CreateParse::Malformed)),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_creation_parent_follows_the_folder_rule() {
        let root = tempfile::tempdir().expect("root");
        let home = root.path().canonicalize().expect("home").join("home");
        let state = home.join("state");
        fs::create_dir_all(&state).expect("state");
        let folders = Folders::new(Some(&home), &[&state]);
        let spelled = |path: &Path| path.to_str().expect("UTF-8").to_owned();
        assert_eq!(folders.create_parent("~/code"), Ok(home.join("code")));
        assert_eq!(
            folders.create_parent(&spelled(&home.join("code/new"))),
            Ok(home.join("code/new"))
        );
        for refused in [
            "~".to_owned(),
            "/".to_owned(),
            spelled(&home),
            "~/.hidden/projects".to_owned(),
            "~/state".to_owned(),
            "~/state/inside".to_owned(),
            "~/code/../other".to_owned(),
            "code".to_owned(),
        ] {
            assert_eq!(
                folders.create_parent(&refused),
                Err(Reason::ParentUnusable),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_wire_shapes_refuse_what_the_page_never_sends() {
        let attempt = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let add = |body: serde_json::Value| parse_add(body.to_string().as_bytes());
        let ok = serde_json::json!({
            "folder": "/x", "check": ["cargo", "test"], "attempt_id": attempt
        });
        assert!(add(ok.clone()).is_some());
        let named = add(serde_json::json!({
            "folder": "/x", "check": "files_changed", "attempt_id": attempt
        }))
        .expect("files changed");
        assert_eq!(named.check.argv(), changed_check());
        for bad in [
            serde_json::json!({"folder": "/x", "check": "make_it_pass", "attempt_id": attempt}),
            serde_json::json!({"folder": "/x", "check": [], "attempt_id": attempt}),
            serde_json::json!({"folder": "/x", "check": ["a"], "attempt_id": "short"}),
            serde_json::json!({"folder": "/x", "check": ["a"], "attempt_id": attempt, "id": "p"}),
            serde_json::json!({"folder": "/x", "check": ["a"], "attempt_id": attempt, "label": ""}),
        ] {
            assert!(add(bad.clone()).is_none(), "{bad}");
        }
        let remove = |body: serde_json::Value| parse_remove(body.to_string().as_bytes());
        assert!(remove(serde_json::json!({"project_id": "app", "attempt_id": attempt})).is_some());
        for bad in [
            serde_json::json!({"project_id": "", "attempt_id": attempt}),
            serde_json::json!({"project_id": "app", "attempt_id": "short"}),
            serde_json::json!({"project_id": "app", "attempt_id": attempt, "folder": "/x"}),
        ] {
            assert!(remove(bad.clone()).is_none(), "{bad}");
        }
        assert!(parse_preview(br#"{"path":"~/code"}"#).is_some());
        assert!(parse_preview(br#"["~/code"]"#).is_none());
    }
}
