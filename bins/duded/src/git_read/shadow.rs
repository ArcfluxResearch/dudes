//! The per-read shadow gitdir: a directory the daemon owns, holding a literal
//! configuration and copies of a repository's small files, and linking only its object stores.
//!
//! Git is run against this directory and never against the agent's own `.git`, so no
//! configuration, hook, attribute file or alternates list the agent wrote is ever opened. Every
//! open of an agent file is `O_NOFOLLOW | O_NONBLOCK` followed by `fstat`, and anything that is not
//! a regular file of the stated size is refused, which is what keeps a FIFO named `HEAD` from
//! blocking the read.
//!
//! A worktree the daemon created has two source directories instead of one. `HEAD` and `index`
//! come from its administrative directory, `<origin>/.git/worktrees/<lease id>`, and everything
//! else from `<origin>/.git`, each opened from the paths the lease records without following a
//! link. The worktree's `.git` file and the administrative directory's `commondir` are agent
//! files, and plain git follows them into another repository. They are never read through: each
//! must name exactly the directory the lease does, compared by device and inode, or the read
//! answers `worktree_mismatch`.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File};
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use dudes_proto::{GitAgainst, GitState};
use dudes_types::GitCommit;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, openat, statat};
use rustix::io::Errno;

use super::{Lease, Tree, parse};

const HEAD_MAX: u64 = 256;
const PACKED_REFS_MAX: u64 = 1024 * 1024;
const LOOSE_REF_MAX: u64 = 256;
const INDEX_MAX: u64 = 32 * 1024 * 1024;
const SHALLOW_MAX: u64 = 64 * 1024;
/// A worktree's `.git` file or its `commondir`: one path and a line end.
const LINK_MAX: u64 = 4 * 1024;
/// Entries, directories included, a walk of the loose refs may visit before the repository is
/// refused as too large for this read.
const REF_ENTRIES_MAX: usize = 2_000;
/// Directory levels under `refs/heads` or `refs/remotes/origin`.
const REF_DEPTH_MAX: usize = 16;

/// The literal configuration of every shadow. Nothing the agent wrote reaches it.
///
/// The pack window and limit are far under the address-space limit a Linux child runs with, which
/// git's default 1 GiB window would otherwise exhaust on a large pack. For the same reason git
/// runs no helper threads: each reserves a stack and a malloc arena, and with the limit in force
/// a large index made `status` fail at random with "unable to create threaded lstat".
const CONFIG: &str = if cfg!(target_os = "macos") {
    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tignorecase = true\n\tprecomposeunicode = true\n\tpackedGitWindowSize = 32m\n\tpackedGitLimit = 256m\n\tpreloadindex = false\n[index]\n\tthreads = false\n"
} else {
    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tpackedGitWindowSize = 32m\n\tpackedGitLimit = 256m\n\tpreloadindex = false\n[index]\n\tthreads = false\n"
};

/// Why a shadow could not be built. Each is a finite state of the reply, with no repository data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    NotACheckoutRoot,
    GitfileUnsupported,
    UnsupportedStorage,
    Unreadable,
    WorktreeMismatch,
}

impl From<Refusal> for GitState {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::NotACheckoutRoot => Self::NotACheckoutRoot,
            Refusal::GitfileUnsupported => Self::GitfileUnsupported,
            Refusal::UnsupportedStorage => Self::UnsupportedStorage,
            Refusal::Unreadable => Self::Unreadable,
            Refusal::WorktreeMismatch => Self::WorktreeMismatch,
        }
    }
}

/// The commit the read compares `HEAD` with.
#[derive(Clone, Copy, Debug)]
pub(super) enum Comparison {
    /// `refs/remotes/origin/HEAD`, the one ref that does not move when the agent pushes its own
    /// branch.
    RemoteDefaultBranch,
    /// The commit the lease recorded when the daemon created the worktree. It comes from the
    /// journal, never from the browser.
    WorktreeStart(GitCommit),
}

impl Comparison {
    /// The `rev-parse --verify` argument that names the commit.
    pub(super) fn revision(self) -> String {
        match self {
            Self::RemoteDefaultBranch => "refs/remotes/origin/HEAD^{commit}".to_owned(),
            Self::WorktreeStart(base) => format!("{base}^{{commit}}"),
        }
    }

    pub(super) fn against(self) -> GitAgainst {
        match self {
            Self::RemoteDefaultBranch => GitAgainst::RemoteDefaultBranch,
            Self::WorktreeStart(_) => GitAgainst::WorktreeStart,
        }
    }
}

/// The opened directories one shadow is filled from.
struct Sources {
    /// Holds `HEAD` and `index`.
    own: OwnedFd,
    /// Holds the refs, `packed-refs`, `shallow` and the object directories.
    common: OwnedFd,
    /// `common`'s object directory as a path, which the shadow's links name.
    objects: PathBuf,
    work_tree: PathBuf,
    comparison: Comparison,
}

impl Sources {
    /// A checkout whose own `.git` holds everything. It must be a real directory: a gitfile, a
    /// link or a missing entry is refused, and no parent directory is searched for a repository.
    fn checkout(cwd: &Path) -> Result<Self, Refusal> {
        let work_tree = fs::canonicalize(cwd).map_err(|_| Refusal::NotACheckoutRoot)?;
        let root = open_dir(rustix::fs::CWD, &work_tree).map_err(|_| Refusal::NotACheckoutRoot)?;
        let Ok(git) = open_dir(&root, ".git") else {
            return Err(classify_git_entry(&root));
        };
        refuse_reftable(&git)?;
        Ok(Self {
            own: git.try_clone().map_err(|_| Refusal::Unreadable)?,
            common: git,
            objects: work_tree.join(".git/objects"),
            work_tree,
            comparison: Comparison::RemoteDefaultBranch,
        })
    }

    /// A worktree the daemon created, opened from the paths its lease records. The origin is
    /// canonicalized as a checkout is; every directory below it, and the worktree itself, must be
    /// a real directory.
    fn lease(lease: &Lease) -> Result<Self, Refusal> {
        let origin = fs::canonicalize(&lease.origin).map_err(|_| Refusal::WorktreeMismatch)?;
        let common = open_dir(rustix::fs::CWD, &origin)
            .and_then(|dir| open_dir(&dir, ".git"))
            .map_err(|_| Refusal::WorktreeMismatch)?;
        let admin = open_dir(&common, "worktrees")
            .and_then(|worktrees| open_dir(&worktrees, lease.worktree.to_string().as_str()))
            .map_err(|_| Refusal::WorktreeMismatch)?;
        refuse_reftable(&common)?;
        refuse_reftable(&admin)?;
        let tree = open_dir(rustix::fs::CWD, &lease.path).map_err(|_| Refusal::WorktreeMismatch)?;
        links_to(&tree, ".git", b"gitdir: ", &admin)?;
        links_to(&admin, "commondir", b"", &common)?;
        Ok(Self {
            own: admin,
            common,
            objects: origin.join(".git/objects"),
            work_tree: lease.path.clone(),
            comparison: Comparison::WorktreeStart(lease.base),
        })
    }
}

/// One built shadow gitdir. Dropping it removes the directory, links as links.
#[derive(Debug)]
pub struct Shadow {
    dir: PathBuf,
    work_tree: PathBuf,
    head: parse::Head,
    comparison: Comparison,
}

impl Shadow {
    /// The shadow directory: the child's `GIT_DIR` and its working directory.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The agent's checkout: the child's `GIT_WORK_TREE`.
    pub(super) fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    pub(super) fn head(&self) -> &parse::Head {
        &self.head
    }

    pub(super) fn comparison(&self) -> Comparison {
        self.comparison
    }

    /// Build a shadow named `name` under `parent` for `tree`.
    ///
    /// `parent` is created `0700` if absent. Every agent file is read without following a link.
    pub(super) fn build(parent: &Path, name: &str, tree: &Tree) -> Result<Self, Refusal> {
        let sources = match tree {
            Tree::Checkout(cwd) => Sources::checkout(cwd)?,
            Tree::Leased(lease) => Sources::lease(lease)?,
        };
        let head_bytes = read_small(&sources.own, "HEAD", HEAD_MAX, Refusal::Unreadable)?
            .ok_or(Refusal::Unreadable)?;
        let head = parse::head(&head_bytes).ok_or(Refusal::Unreadable)?;

        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| Refusal::Unreadable)?;
        let dir = parent.join(name);
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| Refusal::Unreadable)?;
        // From here the guard owns the directory, so every early return removes it.
        let shadow = Self {
            dir,
            work_tree: sources.work_tree.clone(),
            head,
            comparison: sources.comparison,
        };
        shadow.fill(&sources, &head_bytes)?;
        Ok(shadow)
    }

    fn fill(&self, sources: &Sources, head: &[u8]) -> Result<(), Refusal> {
        let put = |name: &str, bytes: &[u8]| fs::write(self.dir.join(name), bytes);
        let io = |_| Refusal::Unreadable;
        put("config", CONFIG.as_bytes()).map_err(io)?;
        put("HEAD", head).map_err(io)?;
        if let Some(packed) = read_small(
            &sources.common,
            "packed-refs",
            PACKED_REFS_MAX,
            Refusal::UnsupportedStorage,
        )? {
            put("packed-refs", &packed).map_err(io)?;
        }
        if let Some((index, modified)) = read_stamped(
            &sources.own,
            "index",
            INDEX_MAX,
            Refusal::UnsupportedStorage,
        )? {
            match parse::check_index(&index) {
                Ok(()) => {}
                Err(parse::IndexRefusal::Version4) => return Err(Refusal::UnsupportedStorage),
                Err(parse::IndexRefusal::Unsafe) => return Err(Refusal::Unreadable),
            }
            put("index", &index).map_err(io)?;
            // Git trusts an entry whose stat matches unless it is as new as the index file, so the
            // copy keeps the original's time. A copy stamped now would make an edit made in the
            // same second as the agent's last index write, to a file of the same size, look clean.
            if let Some(modified) = modified {
                File::options()
                    .write(true)
                    .open(self.dir.join("index"))
                    .and_then(|file| file.set_modified(modified))
                    .map_err(io)?;
            }
        }
        if let Some(shallow) =
            read_small(&sources.common, "shallow", SHALLOW_MAX, Refusal::Unreadable)?
        {
            if !parse::is_shallow(&shallow) {
                return Err(Refusal::Unreadable);
            }
            put("shallow", &shallow).map_err(io)?;
        }

        let refs = open_dir(&sources.common, "refs").map_err(|_| Refusal::Unreadable)?;
        let mut entries = 0;
        if let Some(heads) = open_optional_dir(&refs, "heads")? {
            copy_refs(&heads, &self.dir.join("refs/heads"), &mut entries, 0)?;
        }
        if matches!(self.comparison, Comparison::RemoteDefaultBranch)
            && let Some(remotes) = open_optional_dir(&refs, "remotes")?
            && let Some(origin) = open_optional_dir(&remotes, "origin")?
        {
            copy_refs(
                &origin,
                &self.dir.join("refs/remotes/origin"),
                &mut entries,
                0,
            )?;
        }

        let objects = open_dir(&sources.common, "objects").map_err(|_| Refusal::Unreadable)?;
        let linked = self.dir.join("objects");
        fs::create_dir(&linked).map_err(io)?;
        let mut listing = Dir::read_from(&objects).map_err(|_| Refusal::Unreadable)?;
        while let Some(entry) = listing.read() {
            let entry = entry.map_err(|_| Refusal::Unreadable)?;
            let name = entry.file_name();
            let bytes = name.to_bytes();
            let wanted = bytes == b"pack"
                || (bytes.len() == 2
                    && bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
            if !wanted || entry_type(&objects, &entry) != Some(FileType::Directory) {
                continue;
            }
            let name = std::str::from_utf8(bytes).map_err(|_| Refusal::Unreadable)?;
            symlink(sources.objects.join(name), linked.join(name)).map_err(io)?;
        }
        Ok(())
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn open_dir(parent: impl AsFd, name: impl rustix::path::Arg) -> Result<OwnedFd, Errno> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// A subdirectory that may be absent. Present and not a real directory is a refusal.
fn open_optional_dir(parent: &OwnedFd, name: &str) -> Result<Option<OwnedFd>, Refusal> {
    match open_dir(parent, name) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(_) => Err(Refusal::Unreadable),
    }
}

fn refuse_reftable(dir: &OwnedFd) -> Result<(), Refusal> {
    match statat(dir, "reftable", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Err(Refusal::UnsupportedStorage),
        Err(_) => Ok(()),
    }
}

/// Check that the link file `name` in `dir` names `target`, as git would resolve it: the path
/// after `prefix`, relative to `dir`. The path is followed by `stat` and compared by device and
/// inode. Nothing is opened through it, so all the read learns from it is match or no match.
fn links_to(dir: &OwnedFd, name: &str, prefix: &[u8], target: &OwnedFd) -> Result<(), Refusal> {
    let bytes = read_small(dir, name, LINK_MAX, Refusal::WorktreeMismatch)
        .map_err(|_| Refusal::WorktreeMismatch)?
        .ok_or(Refusal::WorktreeMismatch)?;
    let named = link_path(&bytes, prefix).ok_or(Refusal::WorktreeMismatch)?;
    let there = statat(dir, named, AtFlags::empty()).map_err(|_| Refusal::WorktreeMismatch)?;
    let wanted = fstat(target).map_err(|_| Refusal::WorktreeMismatch)?;
    if (there.st_dev, there.st_ino) == (wanted.st_dev, wanted.st_ino) {
        Ok(())
    } else {
        Err(Refusal::WorktreeMismatch)
    }
}

/// The path a link file holds: everything after `prefix` with the line end removed. Git takes the
/// whole file that way, so a second line would be part of the path git uses, and is refused here.
fn link_path<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a OsStr> {
    let rest = bytes.strip_prefix(prefix)?;
    let end = rest
        .iter()
        .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
        .map_or(0, |last| last + 1);
    let path = &rest[..end];
    let clean = !path.is_empty() && !path.iter().any(|byte| matches!(byte, b'\n' | b'\r' | 0));
    clean.then(|| OsStr::from_bytes(path))
}

/// Why `.git` could not be opened as a real directory.
fn classify_git_entry(root: &OwnedFd) -> Refusal {
    match statat(root, ".git", AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Refusal::NotACheckoutRoot,
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            Refusal::GitfileUnsupported
        }
        _ => Refusal::Unreadable,
    }
}

/// The type of a directory entry, asking the file system when the listing did not say.
fn entry_type(parent: &OwnedFd, entry: &rustix::fs::DirEntry) -> Option<FileType> {
    match entry.file_type() {
        FileType::Unknown => statat(parent, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|stat| FileType::from_raw_mode(stat.st_mode)),
        known => Some(known),
    }
}

/// The bytes of the regular file `name` in `dir`, or `None` when there is none.
///
/// Opened without following a link and without blocking, then checked with `fstat`: a link, a FIFO
/// or a device is refused, and a file over `limit` is refused as `oversize`.
fn read_small(
    dir: &OwnedFd,
    name: &str,
    limit: u64,
    oversize: Refusal,
) -> Result<Option<Vec<u8>>, Refusal> {
    Ok(read_stamped(dir, name, limit, oversize)?.map(|(bytes, _)| bytes))
}

/// A file's bytes and its modification time.
type Stamped = (Vec<u8>, Option<SystemTime>);

/// [`read_small`], with the file's modification time.
fn read_stamped(
    dir: &OwnedFd,
    name: &str,
    limit: u64,
    oversize: Refusal,
) -> Result<Option<Stamped>, Refusal> {
    let fd = match openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(_) => return Err(Refusal::Unreadable),
    };
    read_open(File::from(fd), limit, oversize).map(Some)
}

fn read_open(file: File, limit: u64, oversize: Refusal) -> Result<Stamped, Refusal> {
    let metadata = file.metadata().map_err(|_| Refusal::Unreadable)?;
    if !metadata.is_file() {
        return Err(Refusal::Unreadable);
    }
    if metadata.len() > limit {
        return Err(oversize);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Refusal::Unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(oversize);
    }
    Ok((bytes, metadata.modified().ok()))
}

/// Copy the loose refs under `dir` to `out`, keeping only ref files whose content is one the
/// shadow accepts. Anything that is not a plain directory or a plain file is left out.
fn copy_refs(dir: &OwnedFd, out: &Path, entries: &mut usize, depth: usize) -> Result<(), Refusal> {
    if depth > REF_DEPTH_MAX {
        return Err(Refusal::UnsupportedStorage);
    }
    fs::create_dir_all(out).map_err(|_| Refusal::Unreadable)?;
    let mut listing = Dir::read_from(dir).map_err(|_| Refusal::Unreadable)?;
    while let Some(entry) = listing.read() {
        let entry = entry.map_err(|_| Refusal::Unreadable)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        *entries += 1;
        if *entries > REF_ENTRIES_MAX {
            return Err(Refusal::UnsupportedStorage);
        }
        let Ok(name) = std::str::from_utf8(bytes) else {
            continue;
        };
        if !parse::is_legal_ref_name(name) {
            continue;
        }
        match entry_type(dir, &entry) {
            Some(FileType::Directory) => {
                let child = open_dir(dir, name).map_err(|_| Refusal::Unreadable)?;
                copy_refs(&child, &out.join(name), entries, depth + 1)?;
            }
            Some(FileType::RegularFile) => {
                let Some(content) = read_small(dir, name, LOOSE_REF_MAX, Refusal::Unreadable)?
                else {
                    continue;
                };
                if parse::is_loose_ref(&content) {
                    fs::write(out.join(name), content).map_err(|_| Refusal::Unreadable)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_literal_config_bounds_the_pack_window_under_the_linux_address_space_limit() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        for dir in [".git/objects", ".git/refs"] {
            fs::create_dir_all(checkout.join(dir)).unwrap();
        }
        fs::write(checkout.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let shadow = Shadow::build(
            &root.path().join("scratch"),
            "one",
            &Tree::Checkout(checkout),
        )
        .unwrap();
        let config = fs::read_to_string(shadow.dir().join("config")).unwrap();
        assert!(config.contains("\tpackedGitWindowSize = 32m\n"), "{config}");
        assert!(config.contains("\tpackedGitLimit = 256m\n"), "{config}");
        assert!(config.starts_with("[core]\n"));
        assert_eq!(
            config.matches('[').count(),
            2,
            "the core and index sections, and nothing the agent wrote"
        );
    }
}
