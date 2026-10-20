//! The Floor's add-a-project: the shapes of its two requests and answers, and the rule for which
//! folders it will take.
//!
//! A newcomer types a folder, the page previews it, and one click adds it to the operator's file
//! through the same edit `dudes project add` runs. The daemon then adopts the file at once. The
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

/// What a project job carries.
#[derive(Clone, Debug)]
pub enum ProjectRequest {
    /// Look at a folder and say what adding it would do.
    Preview(PreviewRequest),
    /// Add the folder.
    Add(AddRequest),
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

/// The answer to a project job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectAnswer {
    /// A preview.
    Preview(Preview),
    /// An add, new or repeated.
    Added(Added),
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
        assert!(parse_preview(br#"{"path":"~/code"}"#).is_some());
        assert!(parse_preview(br#"["~/code"]"#).is_none());
    }
}
