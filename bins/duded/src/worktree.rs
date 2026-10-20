//! Directory identity for the worktree overlap refusal.
//!
//! Two paths name one directory when they share a `(device, inode)` pair, and a path is inside a
//! directory when that directory's pair is one of the path's ancestors' pairs. String comparison
//! and `canonicalize` are not enough: a macOS firmlink gives one directory two `canonicalize`d
//! spellings. The supervisor holds recorded paths only, so this runs here, at admission and again
//! on the loop thread immediately before the hire is appended.

use std::path::Path;

use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData, RpcError};
use dudes_types::WorktreeId;

pub mod git;
pub mod hire;
pub mod trust;

/// The refusal for a worktree hire this daemon cannot serve.
pub(crate) fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError {
        code: ErrorCode::Refused.code(),
        message: format!("agent.hire: {}", message.into()),
        data: serde_json::to_value(RefusedData {
            gate: RefusalGate::WorktreeUnavailable,
        })
        .ok(),
    }
}

/// The most bytes of a worktree's `.git` file that are read. Git writes one short line.
#[cfg(unix)]
const GITFILE_LIMIT: u64 = 4096;

/// Whether `path` is still a git worktree of the repository at `origin`: its `.git` is a regular
/// file (never a link, a FIFO or a device) that names, on its first line, an administrative
/// directory under the repository's `<common dir>/worktrees/` that exists.
///
/// Read as bounded text and never by running git, because the agent that works in the tree writes
/// it. A file that is not regular is refused before it is opened, so a FIFO cannot hang the daemon
/// and `/dev/zero` cannot be read into memory.
#[cfg(unix)]
pub(crate) fn is_worktree(path: &Path, origin: &Path) -> bool {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    // Opened without following a link and without blocking, then typed from the descriptor itself,
    // so nothing can be swapped in between the check and the read.
    let Ok(fd) = rustix::fs::open(
        path.join(".git"),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    let regular = rustix::fs::fstat(&fd)
        .is_ok_and(|stat| rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file());
    if !regular {
        return false;
    }
    let mut text = String::new();
    if std::fs::File::from(fd)
        .take(GITFILE_LIMIT)
        .read_to_string(&mut text)
        .is_err()
    {
        return false;
    }
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir: "))
    else {
        return false;
    };
    let Ok(admin) = std::fs::canonicalize(path.join(target.trim())) else {
        return false;
    };
    let Some(common) = plain_git_dir(origin) else {
        return false;
    };
    admin.is_dir() && admin.starts_with(common.join("worktrees"))
}

/// The repository's `.git` directory when the origin is a plain checkout: `.git` is a directory
/// (not a file, as in a linked worktree, a submodule or a `--separate-git-dir` checkout, and not a
/// link). Worktree mode serves only such an origin, because only there is the common directory
/// `<origin>/.git`.
#[cfg(unix)]
pub(crate) fn plain_git_dir(origin: &Path) -> Option<std::path::PathBuf> {
    let dot_git = origin.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    meta.file_type()
        .is_dir()
        .then(|| std::fs::canonicalize(&dot_git).ok())?
}

#[cfg(not(unix))]
pub(crate) fn plain_git_dir(_: &Path) -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(unix))]
pub(crate) fn is_worktree(_: &Path, _: &Path) -> bool {
    true
}

/// Whether `ancestor` is `path` or a directory above it, by `(device, inode)`.
#[cfg(unix)]
pub(crate) fn contains(ancestor: &Path, path: &Path) -> bool {
    let (Some(wanted), Ok(lineage)) = (identity(ancestor), lineage(path)) else {
        return false;
    };
    lineage.chain.contains(&wanted)
}

/// Whether two paths are one directory, by `(device, inode)`.
#[cfg(unix)]
pub(crate) fn same_directory(left: &Path, right: &Path) -> bool {
    matches!((identity(left), identity(right)), (Some(a), Some(b)) if a == b)
}

#[cfg(not(unix))]
pub(crate) fn contains(_: &Path, _: &Path) -> bool {
    true
}

#[cfg(not(unix))]
pub(crate) fn same_directory(_: &Path, _: &Path) -> bool {
    true
}

/// One directory's `(device, inode)`.
#[cfg(unix)]
type Identity = (u64, u64);

/// The directories a path stands on: its own identity when the path exists, then every ancestor's,
/// and the names below its nearest existing ancestor that do not exist yet.
///
/// A path that does not exist yet stands on its nearest existing ancestor, so a hire into
/// `<tree>/new` is still seen to be inside `<tree>`. The missing names are what a lease with no
/// directory yet is compared by. Two such paths can meet only below one shared existing directory,
/// because where they part at a level that exists, the name one of them takes there would exist.
#[cfg(unix)]
struct Lineage {
    own: Option<Identity>,
    chain: Vec<Identity>,
    missing: Vec<String>,
}

/// A directory this check needed could not be resolved (a dangling or looping link, a denied read,
/// a `..` below a directory that does not exist yet). The caller refuses: a hire the daemon cannot
/// place is not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unresolved;

#[cfg(unix)]
fn identity(path: &Path) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(unix)]
fn lineage(path: &Path) -> Result<Lineage, Unresolved> {
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or(Unresolved)?;
            }
            Err(_) => return Err(Unresolved),
        }
    }
    let resolved = std::fs::canonicalize(existing).map_err(|_| Unresolved)?;
    let chain = resolved
        .ancestors()
        .map(|ancestor| identity(ancestor).ok_or(Unresolved))
        .collect::<Result<Vec<Identity>, Unresolved>>()?;
    let missing = path
        .strip_prefix(existing)
        .map_err(|_| Unresolved)?
        .components()
        .map(|part| match part {
            // Folded because a case-insensitive volume, the macOS default, makes `Foo` and `foo`
            // one directory once created. Where the volume is case-sensitive, a false match only
            // refuses a hire.
            std::path::Component::Normal(name) => Ok(name.to_string_lossy().to_lowercase()),
            _ => Err(Unresolved),
        })
        .collect::<Result<Vec<String>, Unresolved>>()?;
    Ok(Lineage {
        own: (existing == path).then(|| chain.first().copied()).flatten(),
        chain,
        missing,
    })
}

/// The first lease in `leases` whose tree `cwd` is at, under or above.
///
/// Identity is `(device, inode)` where both sides exist. Where a lease's tree does not exist yet
/// (its `git worktree add` has not run), it is the identity of the nearest existing directory plus
/// the names below it, without case, so a pending lease still blocks a directory at, under or above
/// where it will be.
///
/// # Errors
///
/// [`Unresolved`] when `cwd` or any lease's tree cannot be placed. This fails closed.
#[cfg(unix)]
pub(crate) fn overlapping<'a>(
    cwd: &Path,
    leases: impl IntoIterator<Item = (WorktreeId, &'a Path)>,
) -> Result<Option<WorktreeId>, Unresolved> {
    let leases: Vec<(WorktreeId, &Path)> = leases.into_iter().collect();
    if leases.is_empty() {
        return Ok(None);
    }
    let candidate = lineage(cwd)?;
    for (id, tree) in leases {
        let tree = lineage(tree)?;
        let inside = tree.own.is_some_and(|own| candidate.chain.contains(&own));
        let above = candidate.own.is_some_and(|own| tree.chain.contains(&own));
        let named = candidate.chain.first() == tree.chain.first()
            && (candidate.missing.starts_with(&tree.missing)
                || tree.missing.starts_with(&candidate.missing));
        if inside || above || named {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Windows has no worktree mode and so no leases to overlap.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix check, whose callers handle `Unresolved`"
)]
pub(crate) fn overlapping<'a>(
    _: &Path,
    _: impl IntoIterator<Item = (WorktreeId, &'a Path)>,
) -> Result<Option<WorktreeId>, Unresolved> {
    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn origin_with_admin(root: &Path, name: &str) -> (PathBuf, PathBuf) {
        let origin = root.join("origin");
        let admin = origin.join(".git/worktrees").join(name);
        fs::create_dir_all(&admin).unwrap();
        (origin, admin)
    }

    #[test]
    fn a_tree_is_a_worktree_only_while_its_gitdir_exists_under_the_origins_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (origin, admin) = origin_with_admin(&root, "one");
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        assert!(!is_worktree(&tree, &origin), "no .git file");
        fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", root.join("gone").display()),
        )
        .unwrap();
        assert!(
            !is_worktree(&tree, &origin),
            "the administrative directory is gone"
        );
        fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        assert!(is_worktree(&tree, &origin));
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();
        assert!(
            !is_worktree(&tree, &origin),
            "a directory outside <common>/worktrees is refused"
        );
        fs::write(tree.join(".git"), "not a gitdir line\n").unwrap();
        assert!(!is_worktree(&tree, &origin));
        assert!(!is_worktree(&root.join("missing"), &origin));
    }

    #[test]
    fn a_gitfile_that_is_not_a_small_regular_file_is_refused_without_being_read() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (origin, admin) = origin_with_admin(&root, "one");
        let line = format!("gitdir: {}\n", admin.display());

        let linked = root.join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(root.join("real-gitfile"), &line).unwrap();
        symlink(root.join("real-gitfile"), linked.join(".git")).unwrap();
        assert!(
            !is_worktree(&linked, &origin),
            "a link to a good file is not the file"
        );

        let zero = root.join("zero");
        fs::create_dir_all(&zero).unwrap();
        symlink("/dev/zero", zero.join(".git")).unwrap();
        let started = std::time::Instant::now();
        assert!(!is_worktree(&zero, &origin));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "/dev/zero was not read"
        );

        // A socket stands for every file that is not regular, a FIFO among them: the check refuses on
        // the file type before it opens anything, so nothing here can block.
        let special = root.join("special");
        fs::create_dir_all(&special).unwrap();
        let _bound = std::os::unix::net::UnixListener::bind(special.join(".git")).unwrap();
        let started = std::time::Instant::now();
        assert!(
            !is_worktree(&special, &origin),
            "a non-regular file is refused"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        let big = root.join("big");
        fs::create_dir_all(&big).unwrap();
        let mut bytes = line.clone().into_bytes();
        bytes.extend(std::iter::repeat_n(b'x', 1 << 20));
        fs::write(big.join(".git"), &bytes).unwrap();
        assert!(
            is_worktree(&big, &origin),
            "only the first line, from the first 4 KiB, matters"
        );
        fs::write(big.join(".git"), format!("{}{line}", "x".repeat(8192))).unwrap();
        assert!(
            !is_worktree(&big, &origin),
            "a gitdir line beyond the read limit is not found"
        );
    }

    #[test]
    fn contains_and_same_directory_compare_by_identity() {
        let f = fixture();
        let alias = f.root.join("repo/alias");
        symlink(&f.tree, &alias).unwrap();
        assert!(contains(&f.root, &f.tree.join("src")));
        assert!(contains(&f.tree, &f.tree));
        assert!(contains(&f.tree, &alias.join("src")));
        assert!(!contains(&f.tree.join("src"), &f.tree));
        assert!(same_directory(&f.tree, &alias));
        assert!(!same_directory(&f.tree, &f.root.join("repo")));
        assert!(!same_directory(&f.tree, &f.root.join("nowhere")));
    }

    fn lease(n: u8) -> WorktreeId {
        WorktreeId::from_bytes([n; 16])
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        tree: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let tree = root.join("data/worktrees/tree");
        fs::create_dir_all(tree.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("data/worktrees/sibling")).unwrap();
        fs::create_dir_all(root.join("repo")).unwrap();
        Fixture {
            _dir: dir,
            root,
            tree,
        }
    }

    fn check(cwd: &Path, tree: &Path) -> Option<WorktreeId> {
        overlapping(cwd, [(lease(1), tree)]).unwrap()
    }

    #[test]
    fn at_under_and_above_a_tree_overlap_and_unrelated_directories_do_not() {
        let f = fixture();
        assert_eq!(check(&f.tree, &f.tree), Some(lease(1)), "at");
        assert_eq!(
            check(&f.tree.join("src/deep"), &f.tree),
            Some(lease(1)),
            "under"
        );
        assert_eq!(
            check(&f.tree.join("src/not-yet"), &f.tree),
            Some(lease(1)),
            "under, missing"
        );
        assert_eq!(
            check(&f.root.join("data"), &f.tree),
            Some(lease(1)),
            "above"
        );
        assert_eq!(check(&f.root, &f.tree), Some(lease(1)), "far above");
        assert_eq!(
            check(&f.root.join("data/worktrees/sibling"), &f.tree),
            None,
            "sibling"
        );
        assert_eq!(check(&f.root.join("repo"), &f.tree), None, "elsewhere");
        assert_eq!(
            check(&f.root.join("data/worktrees/tree-two"), &f.tree),
            None,
            "prefix only"
        );
    }

    #[test]
    fn a_symlink_alias_of_the_tree_or_of_a_directory_above_it_overlaps() {
        let f = fixture();
        let alias = f.root.join("repo/alias");
        symlink(&f.tree, &alias).unwrap();
        assert_eq!(check(&alias, &f.tree), Some(lease(1)), "alias of the tree");
        assert_eq!(
            check(&alias.join("src"), &f.tree),
            Some(lease(1)),
            "under the alias"
        );
        let above = f.root.join("repo/above");
        symlink(f.root.join("data"), &above).unwrap();
        assert_eq!(check(&above, &f.tree), Some(lease(1)), "alias of a parent");
        assert_eq!(
            check(&above.join("worktrees/tree/src"), &f.tree),
            Some(lease(1))
        );
        assert_eq!(
            check(&f.tree, &alias),
            Some(lease(1)),
            "the lease path is the alias"
        );
    }

    #[test]
    fn a_tree_not_created_yet_is_overlapped_from_above_but_not_from_beside() {
        let f = fixture();
        let pending = f.root.join("data/worktrees/pending");
        assert_eq!(check(&f.root.join("data"), &pending), Some(lease(1)));
        assert_eq!(
            check(&f.root.join("data/worktrees"), &pending),
            Some(lease(1))
        );
        assert_eq!(check(&f.root.join("repo"), &pending), None);
        assert_eq!(check(&f.tree, &pending), None);
    }

    #[test]
    fn the_first_overlapped_lease_is_the_one_named() {
        let f = fixture();
        let sibling = f.root.join("data/worktrees/sibling");
        assert_eq!(
            overlapping(
                &sibling,
                [(lease(1), f.tree.as_path()), (lease(2), sibling.as_path())]
            ),
            Ok(Some(lease(2)))
        );
        assert_eq!(overlapping(&sibling, []), Ok(None));
    }

    /// A lease whose directory does not exist yet has no inode, so it is compared by where it
    /// will be. Neither side exists below, which is the case the inode comparison cannot see.
    #[test]
    fn a_pending_lease_with_no_directory_blocks_hires_at_under_and_above_it() {
        let f = fixture();
        let pending = f.root.join("data/worktrees/not-created-yet");
        assert!(!pending.exists(), "a control: the lease has no directory");
        assert_eq!(check(&pending, &pending), Some(lease(1)), "at");
        assert_eq!(
            check(&pending.join("src/deep"), &pending),
            Some(lease(1)),
            "under"
        );
        assert_eq!(
            check(&f.root.join("data/worktrees"), &pending),
            Some(lease(1)),
            "above"
        );
        assert_eq!(
            check(&f.root.join("data/worktrees/other"), &pending),
            None,
            "beside"
        );
        assert_eq!(
            check(&f.root.join("data/worktrees/not-created-yet-two"), &pending),
            None,
            "a longer name is not under it"
        );
    }

    #[test]
    fn with_no_lease_to_overlap_nothing_is_resolved_and_nothing_is_refused() {
        let nowhere = Path::new("relative/and/not/there");
        assert_eq!(overlapping(nowhere, []), Ok(None));
        let f = fixture();
        assert_eq!(
            overlapping(nowhere, [(lease(1), f.tree.as_path())]),
            Err(Unresolved),
            "a control: with a lease to compare against, the same path is not placeable"
        );
    }

    #[test]
    fn what_cannot_be_resolved_is_not_admitted() {
        let f = fixture();
        let dangling = f.root.join("repo/dangling");
        symlink(f.root.join("nowhere"), &dangling).unwrap();
        assert_eq!(
            overlapping(&dangling, [(lease(1), f.tree.as_path())]),
            Err(Unresolved)
        );
        assert_eq!(
            overlapping(&f.root.join("repo"), [(lease(1), dangling.as_path())]),
            Err(Unresolved),
            "a lease whose tree cannot be placed refuses every hire"
        );
        let looped = f.root.join("repo/loop");
        symlink(&looped, &looped).unwrap();
        assert_eq!(
            overlapping(&looped, [(lease(1), f.tree.as_path())]),
            Err(Unresolved)
        );
        let locked = f.root.join("repo/locked");
        fs::create_dir_all(locked.join("inner")).unwrap();
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let denied = overlapping(&locked.join("inner/x"), [(lease(1), f.tree.as_path())]);
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        if fs::read_dir(&locked).is_ok()
            && std::env::var_os("USER").is_some_and(|user| user == "root")
        {
            return;
        }
        assert_eq!(denied, Err(Unresolved), "a denied read is not a pass");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlink_spelling_of_the_same_directory_overlaps() {
        let f = fixture();
        let data = Path::new("/System/Volumes/Data");
        let Ok(relative) = f.tree.strip_prefix("/") else {
            return;
        };
        let alias = data.join(relative);
        if !alias.exists() {
            return;
        }
        assert_ne!(alias, f.tree);
        assert_eq!(
            check(&alias, &f.tree),
            Some(lease(1)),
            "alias hires into the tree"
        );
        assert_eq!(
            check(&f.tree, &alias),
            Some(lease(1)),
            "tree hires into the alias lease"
        );
        assert_eq!(check(&alias.join("src"), &f.tree), Some(lease(1)), "under");
        let above = data.join(f.root.strip_prefix("/").unwrap());
        assert_eq!(check(&above, &f.tree), Some(lease(1)), "above");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlink_spelling_of_a_pending_lease_overlaps() {
        let f = fixture();
        let pending = f.root.join("data/worktrees/pending");
        let alias = Path::new("/System/Volumes/Data").join(pending.strip_prefix("/").unwrap());
        if !alias.parent().is_some_and(Path::exists) {
            return;
        }
        assert_eq!(check(&alias.join("src"), &pending), Some(lease(1)), "under");
        assert_eq!(
            check(&pending.join("src"), &alias),
            Some(lease(1)),
            "the lease is spelled through the firmlink"
        );
    }

    #[test]
    fn a_missing_name_matches_a_pending_lease_in_any_case() {
        let f = fixture();
        let pending = f.root.join("data/worktrees/pending");
        assert_eq!(
            check(&f.root.join("data/worktrees/PENDING/src"), &pending),
            Some(lease(1)),
            "under"
        );
        assert_eq!(
            check(&f.root.join("data/worktrees/Pending"), &pending),
            Some(lease(1)),
            "at"
        );
        assert_eq!(
            check(&f.root.join("data/New"), &f.root.join("data/new/pending")),
            Some(lease(1)),
            "above"
        );
        assert_eq!(
            check(&f.root.join("data/worktrees/PENDING-two"), &pending),
            None,
            "a longer name is still beside it"
        );
    }

    #[test]
    fn a_parent_step_below_the_nearest_existing_directory_is_not_placed() {
        let f = fixture();
        let pending = f.root.join("data/worktrees/pending");
        let stepped = f.root.join("data/worktrees/zzz/../pending/src");
        assert_eq!(
            overlapping(&stepped, [(lease(1), pending.as_path())]),
            Err(Unresolved),
            "the hire"
        );
        assert_eq!(
            overlapping(&pending.join("src"), [(lease(1), stepped.as_path())]),
            Err(Unresolved),
            "the lease"
        );
        assert_eq!(
            check(&f.root.join("repo/../data/worktrees/pending/src"), &pending),
            Some(lease(1)),
            "a parent step through directories that exist is resolved"
        );
    }
}
