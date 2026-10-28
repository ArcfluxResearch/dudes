//! The Floor's creation of a new project: a folder made beneath the parent the operator's file
//! names, a git repository started in it, and the folder enrolled like an added one.
//!
//! It runs on the caller's connection thread, like the worktree hire: the loop answers two short
//! jobs (what the file says, then the enrollment) and never waits on the file system or on git.
//! The thread holds the reload lock from the first job to the answer, so a competing add, remove
//! or reload meets `busy`.
//!
//! The folder is never the page's to name. The page sends a child name, and the folder is made
//! with `mkdirat` beneath a parent opened one component at a time without following links.
//!
//! # When the folder is removed
//!
//! Only before the operator's file is touched, and only if the folder is still the one this
//! attempt made and holds only what the repository's start left. Once the enrollment begins to
//! replace the file the new project may be published, so from then on no failure removes the
//! folder, even when the file is put back. The operator is told the path either way.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use dudes_proto::record::Hex32;

use crate::floor_project::{Added, ConfigState, CreateRequest, Reason};
use crate::floor_session::SessionIdentity;
use crate::ports::Handle;

/// Most finished creations a session remembers, so a retry after a lost answer gets the answer
/// the first try earned instead of finding the folder in the way.
const MOST_RESULTS: usize = 32;

/// Most sessions whose results are remembered at once.
const MOST_SESSIONS: usize = 64;

/// Where a folder the operator has to deal with was left.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "only a Unix creation makes a folder")
)]
pub(crate) struct LeftBehind {
    pub(crate) path: PathBuf,
    /// False when the folder or its parent could not be confirmed to be the ones this attempt
    /// made, so the path names a place that may hold something else.
    pub(crate) verified: bool,
}

/// A project that now exists.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "only a Unix creation makes a folder")
)]
pub(crate) struct Created {
    pub(crate) added: Added,
    pub(crate) folder: PathBuf,
}

/// A creation that did not finish.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "only a Unix creation makes a folder")
)]
pub(crate) struct Refused {
    /// The fixed code the page shows.
    pub(crate) code: &'static str,
    /// The folder still on disk, when there is one.
    pub(crate) left_behind: Option<LeftBehind>,
    /// What the operator's file holds, when the enrollment reached it.
    pub(crate) config: Option<ConfigState>,
}

impl Refused {
    fn plain(reason: Reason) -> Self {
        Self {
            code: reason.code(),
            left_behind: None,
            config: None,
        }
    }
}

/// How a creation ended.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "only a Unix creation makes a folder")
)]
pub(crate) enum Outcome {
    Created(Created),
    Refused(Refused),
    Forbidden,
    Unavailable,
}

/// An outcome, and whether this attempt made a folder. Only such an outcome is remembered: a
/// refusal that touched nothing is safe to try again, and a retry may even want to.
#[derive(Clone, Debug)]
pub(crate) struct Done {
    pub(crate) outcome: Outcome,
    pub(crate) made_folder: bool,
}

impl Done {
    fn untouched(outcome: Outcome) -> Self {
        Self {
            outcome,
            made_folder: false,
        }
    }

    fn refused(reason: Reason) -> Self {
        Self::untouched(Outcome::Refused(Refused::plain(reason)))
    }
}

/// What the results hold for an attempt.
pub(crate) enum Lookup {
    Hit(Outcome),
    /// The attempt id was used for a different request.
    Mismatch,
    Miss,
}

/// A session's finished creations, oldest first: attempt, the request's digest and the outcome.
type Finished = VecDeque<(Hex32, Hex32, Outcome)>;

/// The finished creations of each live session, for the length of the run.
#[derive(Debug, Default)]
pub(crate) struct CreateResults {
    sessions: Mutex<BTreeMap<[u8; 32], Finished>>,
}

impl CreateResults {
    pub(crate) fn lookup(
        &self,
        session: &SessionIdentity,
        attempt: Hex32,
        digest: Hex32,
    ) -> Lookup {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if !session.is_valid() {
            sessions.remove(&session.limit_key());
            return Lookup::Miss;
        }
        sessions
            .get(&session.limit_key())
            .and_then(|results| results.iter().find(|(id, ..)| *id == attempt))
            .map_or(Lookup::Miss, |(_, found, outcome)| {
                if *found == digest {
                    Lookup::Hit(outcome.clone())
                } else {
                    Lookup::Mismatch
                }
            })
    }

    pub(crate) fn remember(
        &self,
        session: &SessionIdentity,
        attempt: Hex32,
        digest: Hex32,
        outcome: Outcome,
    ) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if !session.is_valid() {
            sessions.remove(&session.limit_key());
            return;
        }
        if sessions.len() >= MOST_SESSIONS && !sessions.contains_key(&session.limit_key()) {
            sessions.pop_first();
        }
        let results = sessions.entry(session.limit_key()).or_default();
        if results.len() >= MOST_RESULTS {
            results.pop_front();
        }
        results.push_back((attempt, digest, outcome));
    }
}

/// Create the project `request` asks for, on behalf of `session`.
pub(crate) fn run(handle: &Handle, session: &SessionIdentity, request: &CreateRequest) -> Done {
    #[cfg(unix)]
    {
        unix::run(handle, session, request)
    }
    #[cfg(not(unix))]
    {
        let _ = (handle, session, request);
        Done::refused(Reason::NoParent)
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::{CString, OsStr, OsString};
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};
    use std::sync::TryLockError;
    use std::time::Duration;

    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

    use super::{
        ConfigState, CreateRequest, Created, Done, LeftBehind, Outcome, Reason, Refused,
        SessionIdentity,
    };
    use crate::floor_project::{Enroll, EnrollFailure, ProjectAnswer, ProjectRequest, Refusal};
    use crate::ports::Handle;
    use crate::worktree::git::{Git, GitError};

    /// `--initial-branch` arrived in git 2.28.
    const MINIMUM_GIT: (u32, u32) = (2, 28);

    /// How long any one git step may take. A new repository takes milliseconds.
    const STEP: Duration = Duration::from_secs(30);

    const FIRST_COMMIT: &str = "Initialize project";
    const AUTHOR: &str = "Dudes <dudes@localhost>";

    /// Directories deeper than a fresh repository's are not ours to walk.
    const MOST_DEPTH: usize = 6;

    pub(super) fn run(handle: &Handle, session: &SessionIdentity, request: &CreateRequest) -> Done {
        let Some(git) = handle.git() else {
            return Done::refused(Reason::GitUnavailable);
        };
        if git.check_at_least(MINIMUM_GIT).is_err() {
            return Done::refused(Reason::GitUnavailable);
        }
        let lock = handle.reload_lock();
        let mut sequence = match lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Done::refused(Reason::Busy),
        };
        *sequence += 1;
        let seq = *sequence;
        let done = create(handle, session, request, &git, seq);
        drop(sequence);
        done
    }

    fn create(
        handle: &Handle,
        session: &SessionIdentity,
        request: &CreateRequest,
        git: &Git,
        seq: u64,
    ) -> Done {
        let plan = match handle.project(session.clone(), ProjectRequest::CreatePlan) {
            Ok(ProjectAnswer::Plan(plan)) => plan,
            Ok(_) => return Done::refused(Reason::CreateFailed),
            Err(refusal) => return Done::untouched(gate(refusal)),
        };
        let Ok(parent) = Parent::open(&plan.parent) else {
            return Done::refused(Reason::ParentUnusable);
        };
        let child = match parent.make_child(request.name.as_str()) {
            Ok(child) => child,
            Err(MakeError::Exists) => return Done::refused(Reason::Exists),
            Err(MakeError::Unusable) => return Done::refused(Reason::ParentUnusable),
        };
        let made = |outcome| Done {
            outcome,
            made_folder: true,
        };

        if let Err(error) = start_repository(git, &child.path) {
            tracing::warn!(%error, "floor project create: the repository did not start");
            return made(Outcome::Refused(cleaned(
                &parent,
                &child,
                Reason::CreateFailed,
                ConfigState::Unchanged,
            )));
        }
        if !parent.is_at_path() || !parent.holds(&child) {
            return made(Outcome::Refused(Refused {
                code: Reason::CreateFailed.code(),
                left_behind: Some(LeftBehind {
                    path: child.path.clone(),
                    verified: false,
                }),
                config: None,
            }));
        }

        let enroll = Enroll {
            folder: child.path.clone(),
            label: request
                .label
                .clone()
                .unwrap_or_else(|| request.name.as_str().to_owned()),
            config_sha: plan.config_sha,
            seq,
        };
        match handle.project(session.clone(), ProjectRequest::CreateEnroll(enroll)) {
            Ok(ProjectAnswer::Enrolled(Ok(added))) => made(Outcome::Created(Created {
                added,
                folder: child.path,
            })),
            Ok(ProjectAnswer::Enrolled(Err(EnrollFailure {
                reason,
                replaced: false,
                ..
            }))) => made(Outcome::Refused(cleaned(
                &parent,
                &child,
                reason,
                ConfigState::Unchanged,
            ))),
            Ok(ProjectAnswer::Enrolled(Err(EnrollFailure { reason, config, .. }))) => {
                made(Outcome::Refused(Refused {
                    code: reason.code(),
                    left_behind: Some(LeftBehind {
                        path: child.path,
                        verified: true,
                    }),
                    config: Some(config),
                }))
            }
            Ok(_) => made(Outcome::Refused(cleaned(
                &parent,
                &child,
                Reason::CreateFailed,
                ConfigState::Unchanged,
            ))),
            Err(Refusal::Unavailable) => made(Outcome::Refused(Refused {
                code: Reason::ConfigChanged.code(),
                left_behind: Some(LeftBehind {
                    path: child.path,
                    verified: false,
                }),
                config: Some(ConfigState::Unknown),
            })),
            Err(refusal) => {
                let mut refused = cleaned(
                    &parent,
                    &child,
                    Reason::CreateFailed,
                    ConfigState::Unchanged,
                );
                match gate(refusal) {
                    Outcome::Refused(gated) => refused.code = gated.code,
                    other if refused.left_behind.is_none() => return Done::untouched(other),
                    _ => {}
                }
                made(Outcome::Refused(refused))
            }
        }
    }

    /// The outcome for a job the loop turned away before it changed anything.
    fn gate(refusal: Refusal) -> Outcome {
        match refusal {
            Refusal::Reason(reason) => Outcome::Refused(Refused::plain(reason)),
            Refusal::ActionOff => Outcome::Refused(Refused {
                code: crate::floor_switches::ACTION_OFF,
                left_behind: None,
                config: None,
            }),
            Refusal::Forbidden => Outcome::Forbidden,
            Refusal::Unavailable => Outcome::Unavailable,
        }
    }

    /// The refusal after a failure before the file was touched: the folder removed when it is
    /// still the one this attempt made and holds what the start left, else left and named.
    fn cleaned(parent: &Parent, child: &Child, reason: Reason, config: ConfigState) -> Refused {
        let verified = parent.is_at_path() && parent.holds(child);
        let removed = verified && parent.remove(child).is_ok();
        Refused {
            code: reason.code(),
            left_behind: (!removed).then(|| LeftBehind {
                path: child.path.clone(),
                verified,
            }),
            config: Some(config),
        }
    }

    /// Start the repository in `folder` and check what was made.
    fn start_repository(git: &Git, folder: &Path) -> Result<(), GitError> {
        let template = EmptyTemplate::new()?;
        let mut template_flag = OsString::from("--template=");
        template_flag.push(template.path());
        // Identity and signing are set on the command, never read from a configuration.
        let identity: [&OsStr; 6] = [
            OsStr::new("-c"),
            OsStr::new("user.name=Dudes"),
            OsStr::new("-c"),
            OsStr::new("user.email=dudes@localhost"),
            OsStr::new("-c"),
            OsStr::new("commit.gpgSign=false"),
        ];
        let step = |tail: &[&OsStr]| -> Result<String, GitError> {
            let args: Vec<&OsStr> = identity
                .iter()
                .copied()
                .chain(tail.iter().copied())
                .collect();
            git.exec(folder, &args, STEP)
        };
        step(&[
            OsStr::new("init"),
            OsStr::new("--initial-branch=main"),
            template_flag.as_os_str(),
            OsStr::new("."),
        ])?;
        step(&[
            OsStr::new("commit"),
            OsStr::new("--allow-empty"),
            OsStr::new("--no-verify"),
            OsStr::new("--no-gpg-sign"),
            OsStr::new("-m"),
            OsStr::new(FIRST_COMMIT),
        ])?;
        let wrong =
            |what: &str| GitError::Failed(format!("the new repository's {what} is not as made"));
        if step(&[
            OsStr::new("symbolic-ref"),
            OsStr::new("--short"),
            OsStr::new("HEAD"),
        ])? != "main"
        {
            return Err(wrong("branch"));
        }
        let facts = step(&[
            OsStr::new("log"),
            OsStr::new("-n"),
            OsStr::new("1"),
            OsStr::new("--format=%an <%ae>%n%cn <%ce>%n%s%n%T"),
        ])?;
        let mut lines = facts.lines();
        let (author, committer, subject, tree) =
            (lines.next(), lines.next(), lines.next(), lines.next());
        if author != Some(AUTHOR) || committer != Some(AUTHOR) {
            return Err(wrong("author"));
        }
        if subject != Some(FIRST_COMMIT) || lines.next().is_some() {
            return Err(wrong("commit message"));
        }
        let empty = step(&[
            OsStr::new("hash-object"),
            OsStr::new("-t"),
            OsStr::new("tree"),
            OsStr::new("/dev/null"),
        ])?;
        if tree != Some(empty.as_str()) {
            return Err(wrong("tree"));
        }
        if step(&[
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new("HEAD"),
        ])? != "1"
        {
            return Err(wrong("history"));
        }
        git.commit(folder, "HEAD").map(|_| ())
    }

    /// A directory with nothing in it, for `git init` to copy instead of any template of its own.
    struct EmptyTemplate(PathBuf);

    impl EmptyTemplate {
        fn new() -> Result<Self, GitError> {
            use std::fmt::Write as _;
            use std::os::unix::fs::DirBuilderExt as _;
            let mut salt = [0u8; 8];
            getrandom::fill(&mut salt).expect("the operating system's entropy source");
            let mut name = String::from("dudes-empty-template-");
            for byte in salt {
                let _ = write!(name, "{byte:02x}");
            }
            let path = std::env::temp_dir().join(name);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .map_err(|error| GitError::Failed(format!("no empty template: {error}")))?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for EmptyTemplate {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    /// Why a child could not be made.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum MakeError {
        Exists,
        Unusable,
    }

    /// What names a directory: where it lives, and which one it is.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Identity {
        device: u64,
        inode: u64,
    }

    fn identity_of(stat: &rustix::fs::Stat) -> Identity {
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_lossless,
            clippy::unnecessary_cast,
            reason = "st_dev and st_ino are signed on some targets and unsigned on others; only equality matters"
        )]
        Identity {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// The folder new projects are made in, open, and who it was when it was opened.
    pub(super) struct Parent {
        fd: OwnedFd,
        path: PathBuf,
        id: Identity,
    }

    /// A folder this attempt made.
    pub(super) struct Child {
        fd: OwnedFd,
        name: CString,
        pub(super) path: PathBuf,
        id: Identity,
    }

    /// `path` opened one component at a time, none followed through a link.
    fn open_unlinked(path: &Path) -> Result<(OwnedFd, Identity), ()> {
        use std::path::Component;
        let mut fd = rustix::fs::open("/", DIRECTORY.difference(OFlags::NOFOLLOW), Mode::empty())
            .map_err(|_| ())?;
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = rustix::fs::openat(&fd, name, DIRECTORY, Mode::empty()).map_err(|_| ())?;
                }
                _ => return Err(()),
            }
        }
        let id = identity_of(&rustix::fs::fstat(&fd).map_err(|_| ())?);
        Ok((fd, id))
    }

    impl Parent {
        /// Open `path`, which must be its own canonical spelling: no link, no case that differs.
        pub(super) fn open(path: &Path) -> Result<Self, ()> {
            if std::fs::canonicalize(path).is_ok_and(|canonical| canonical != path) {
                return Err(());
            }
            let (fd, id) = open_unlinked(path)?;
            Ok(Self {
                fd,
                path: path.to_path_buf(),
                id,
            })
        }

        /// Whether the spelled path still reaches the folder that was opened.
        pub(super) fn is_at_path(&self) -> bool {
            open_unlinked(&self.path).is_ok_and(|(_, id)| id == self.id)
        }

        /// Make `name` here, which must not exist in any form, with mode `0700`.
        pub(super) fn make_child(&self, name: &str) -> Result<Child, MakeError> {
            let c_name = CString::new(name).map_err(|_| MakeError::Unusable)?;
            rustix::fs::mkdirat(&self.fd, &c_name, Mode::from_raw_mode(0o700)).map_err(
                |error| {
                    if error == rustix::io::Errno::EXIST {
                        MakeError::Exists
                    } else {
                        MakeError::Unusable
                    }
                },
            )?;
            let opened =
                rustix::fs::openat(&self.fd, &c_name, DIRECTORY, Mode::empty()).and_then(|fd| {
                    // The mode given to mkdir is cut by the process's umask.
                    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
                    let stat = rustix::fs::fstat(&fd)?;
                    Ok((fd, identity_of(&stat)))
                });
            let Ok((fd, id)) = opened else {
                let _ = rustix::fs::unlinkat(&self.fd, &c_name, AtFlags::REMOVEDIR);
                return Err(MakeError::Unusable);
            };
            Ok(Child {
                fd,
                path: self.path.join(name),
                name: c_name,
                id,
            })
        }

        /// Whether `child`'s name here still reaches the folder this attempt made.
        pub(super) fn holds(&self, child: &Child) -> bool {
            rustix::fs::statat(&self.fd, &child.name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                FileType::from_raw_mode(stat.st_mode) == FileType::Directory
                    && identity_of(&stat) == child.id
            })
        }

        /// Remove `child` if it holds nothing but a `.git` directory, going down by handle and
        /// never through a link. Anything else, or any failure, leaves what is left.
        pub(super) fn remove(&self, child: &Child) -> Result<(), ()> {
            if !self.holds(child) {
                return Err(());
            }
            let git = c".git";
            let mut seen_git = false;
            for entry in Dir::read_from(&child.fd).map_err(|_| ())? {
                let entry = entry.map_err(|_| ())?;
                let name = entry.file_name();
                if name == c"." || name == c".." {
                    continue;
                }
                if name != git {
                    return Err(());
                }
                seen_git = true;
            }
            if seen_git {
                remove_tree(&child.fd, git, 0)?;
            }
            rustix::fs::unlinkat(&self.fd, &child.name, AtFlags::REMOVEDIR).map_err(|_| ())
        }
    }

    fn remove_tree(parent: &OwnedFd, name: &std::ffi::CStr, depth: usize) -> Result<(), ()> {
        if depth > MOST_DEPTH {
            return Err(());
        }
        let dir = rustix::fs::openat(parent, name, DIRECTORY, Mode::empty()).map_err(|_| ())?;
        let mut names = Vec::new();
        for entry in Dir::read_from(&dir).map_err(|_| ())? {
            let entry = entry.map_err(|_| ())?;
            let found = entry.file_name();
            if found != c"." && found != c".." {
                names.push(found.to_owned());
            }
        }
        for found in names {
            let stat =
                rustix::fs::statat(&dir, &found, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| ())?;
            if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                remove_tree(&dir, &found, depth + 1)?;
            } else {
                rustix::fs::unlinkat(&dir, &found, AtFlags::empty()).map_err(|_| ())?;
            }
        }
        rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|_| ())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn canonical_root() -> (tempfile::TempDir, PathBuf) {
            let root = tempfile::tempdir().expect("root");
            let path = root.path().canonicalize().expect("canonical");
            (root, path)
        }

        /// Only a folder that is still the one made, holding only `.git`, is removed.
        #[test]
        fn cleanup_removes_only_what_the_attempt_made() {
            let (_root, path) = canonical_root();
            let parent = Parent::open(&path).expect("parent");

            let child = parent.make_child("app").expect("child");
            let mode = std::fs::metadata(&child.path).expect("metadata");
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
                0o700
            );
            std::fs::create_dir_all(child.path.join(".git/objects/ab")).expect("repository");
            std::fs::write(child.path.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
            std::fs::write(child.path.join(".git/objects/ab/cd"), "x").expect("object");
            parent.remove(&child).expect("removed");
            assert!(!path.join("app").exists());

            let child = parent.make_child("kept").expect("child");
            std::fs::write(child.path.join("notes.txt"), "mine").expect("note");
            assert!(parent.remove(&child).is_err(), "an extra file");
            assert!(path.join("kept/notes.txt").exists());

            let child = parent.make_child("swapped").expect("child");
            std::fs::remove_dir(&child.path).expect("rmdir");
            std::fs::create_dir(&child.path).expect("another folder in its place");
            assert!(!parent.holds(&child));
            assert!(parent.remove(&child).is_err(), "not the folder made");
            assert!(path.join("swapped").exists());
        }

        /// A link inside the new repository is removed as a link, never followed.
        #[test]
        fn cleanup_never_follows_a_link() {
            let (_root, path) = canonical_root();
            let outside = path.join("outside");
            std::fs::create_dir(&outside).expect("outside");
            std::fs::write(outside.join("keep"), "x").expect("file");
            let parent = Parent::open(&path).expect("parent");
            let child = parent.make_child("app").expect("child");
            std::fs::create_dir(child.path.join(".git")).expect("git");
            std::os::unix::fs::symlink(&outside, child.path.join(".git/link")).expect("link");
            parent.remove(&child).expect("removed");
            assert!(!path.join("app").exists());
            assert!(
                outside.join("keep").exists(),
                "the link's target was touched"
            );
        }

        #[test]
        fn a_child_that_exists_in_any_form_is_not_made() {
            let (_root, path) = canonical_root();
            let parent = Parent::open(&path).expect("parent");
            std::fs::create_dir(path.join("dir")).expect("dir");
            std::fs::write(path.join("file"), "x").expect("file");
            std::os::unix::fs::symlink(path.join("nowhere"), path.join("dangling")).expect("link");
            for name in ["dir", "file", "dangling"] {
                assert_eq!(
                    parent.make_child(name).err(),
                    Some(MakeError::Exists),
                    "{name}"
                );
            }
            assert!(
                !path.join("nowhere").exists(),
                "a dangling link was followed"
            );
        }

        #[test]
        fn a_parent_spelled_through_a_link_is_not_opened() {
            let (_root, path) = canonical_root();
            std::fs::create_dir_all(path.join("real/inner")).expect("real");
            std::os::unix::fs::symlink(path.join("real"), path.join("link")).expect("link");
            assert!(Parent::open(&path.join("real/inner")).is_ok());
            assert!(Parent::open(&path.join("link")).is_err());
            assert!(Parent::open(&path.join("link/inner")).is_err());
            assert!(Parent::open(&path.join("missing")).is_err());
        }

        #[test]
        fn a_parent_that_moved_is_noticed() {
            let (_root, path) = canonical_root();
            std::fs::create_dir(path.join("home")).expect("home");
            let parent = Parent::open(&path.join("home")).expect("parent");
            assert!(parent.is_at_path());
            std::fs::rename(path.join("home"), path.join("moved")).expect("move");
            std::fs::create_dir(path.join("home")).expect("a new folder under the old name");
            assert!(!parent.is_at_path());
        }
    }
}
