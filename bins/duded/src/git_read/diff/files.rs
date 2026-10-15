use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
use rustix::io::Errno;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{RepoPath, RootIdentity};
use dudes_proto::{DiffOmissionReason, DiffRefusal, MAX_DIFF_SIDE_BYTES};

#[derive(Clone)]
pub(super) struct Budget {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Budget {
    pub(super) fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + super::super::READ_DEADLINE,
            cancelled,
        }
    }
    pub(super) fn check(&self) -> Result<(), DiffRefusal> {
        if Instant::now() >= self.deadline || self.cancelled.load(Ordering::Acquire) {
            Err(DiffRefusal::TimedOut)
        } else {
            Ok(())
        }
    }
    pub(super) fn remaining(&self) -> Result<Duration, DiffRefusal> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
    #[cfg(test)]
    pub(super) fn until(deadline: Instant, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline,
            cancelled,
        }
    }
}

pub(super) fn open_dir(parent: impl AsFd, name: impl rustix::path::Arg) -> Result<OwnedFd, Errno> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[allow(
    clippy::useless_conversion,
    reason = "device and inode field widths differ across Unix targets"
)]
fn stat_identity(stat: &rustix::fs::Stat) -> Option<RootIdentity> {
    Some(RootIdentity {
        device: stat.st_dev.try_into().ok()?,
        inode: stat.st_ino.try_into().ok()?,
    })
}

pub(super) fn identity(fd: impl AsFd) -> Result<RootIdentity, DiffRefusal> {
    let stat = rustix::fs::fstat(fd).map_err(|_| DiffRefusal::Unreadable)?;
    stat_identity(&stat).ok_or(DiffRefusal::Unreadable)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    identity: RootIdentity,
    size: u64,
    mode: u32,
    regular: bool,
    modified: (i64, i64),
    changed: (i64, i64),
    links: u64,
}

impl Stamp {
    pub(super) fn file(file: &File) -> Result<Self, DiffRefusal> {
        let meta = file.metadata().map_err(|_| DiffRefusal::Unreadable)?;
        Ok(Self {
            identity: RootIdentity {
                device: meta.dev(),
                inode: meta.ino(),
            },
            size: meta.len(),
            mode: meta.mode(),
            regular: meta.is_file(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
            links: meta.nlink(),
        })
    }
}

pub(super) struct Root {
    path: PathBuf,
    fd: OwnedFd,
    identity: RootIdentity,
}

pub(super) struct CurrentFile {
    pub(super) bytes: Vec<u8>,
    pub(super) mode: u32,
}

impl Root {
    pub(super) fn open(
        path: &Path,
        expected: Option<RootIdentity>,
        budget: &Budget,
    ) -> Result<Self, DiffRefusal> {
        budget.check()?;
        let fd = open_dir(rustix::fs::CWD, path).map_err(|_| DiffRefusal::RootMismatch)?;
        let identity = identity(&fd)?;
        if expected.is_some_and(|expected| expected != identity) {
            return Err(DiffRefusal::RootMismatch);
        }
        let root = Self {
            path: path.to_path_buf(),
            fd,
            identity,
        };
        root.verify()?;
        budget.check()?;
        Ok(root)
    }
    pub(super) fn identity(&self) -> RootIdentity {
        self.identity
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn fd(&self) -> &OwnedFd {
        &self.fd
    }
    pub(super) fn verify(&self) -> Result<(), DiffRefusal> {
        let stat = statat(rustix::fs::CWD, &self.path, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| DiffRefusal::RootMismatch)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || stat_identity(&stat) != Some(self.identity)
        {
            return Err(DiffRefusal::RootMismatch);
        }
        Ok(())
    }
    pub(super) fn read_file(
        &self,
        path: &RepoPath,
        budget: &Budget,
    ) -> Result<Option<CurrentFile>, DiffOmissionReason> {
        self.sample_with(path, budget, || {})
    }
    fn verify_parents(
        &self,
        parents: &[(OwnedFd, String, RootIdentity)],
        budget: &Budget,
    ) -> Result<(), DiffOmissionReason> {
        for (parent, name, id) in parents {
            let named = statat(parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| DiffOmissionReason::Raced)?;
            if FileType::from_raw_mode(named.st_mode) != FileType::Directory
                || stat_identity(&named) != Some(*id)
            {
                return Err(DiffOmissionReason::Raced);
            }
        }
        self.verify().map_err(|_| DiffOmissionReason::Raced)?;
        budget.check().map_err(|_| DiffOmissionReason::Unreadable)
    }
    fn absent(
        &self,
        parent: &OwnedFd,
        name: &str,
        parents: &[(OwnedFd, String, RootIdentity)],
        budget: &Budget,
        after_read: impl FnOnce(),
    ) -> Result<Option<CurrentFile>, DiffOmissionReason> {
        after_read();
        if !matches!(
            statat(parent, name, AtFlags::SYMLINK_NOFOLLOW),
            Err(Errno::NOENT)
        ) {
            return Err(DiffOmissionReason::Raced);
        }
        self.verify_parents(parents, budget)?;
        Ok(None)
    }
    pub(super) fn untracked(
        &self,
        tracked: &BTreeSet<Vec<u8>>,
        budget: &Budget,
    ) -> Result<(u32, bool), DiffRefusal> {
        let mut count = 0_u32;
        let mut cut = false;
        let mut pending = vec![(
            self.fd.try_clone().map_err(|_| DiffRefusal::Unreadable)?,
            Vec::new(),
        )];
        while let Some((parent, prefix)) = pending.pop() {
            budget.check()?;
            let mut listing =
                rustix::fs::Dir::read_from(&parent).map_err(|_| DiffRefusal::Unreadable)?;
            while let Some(entry) = listing.read() {
                budget.check()?;
                let entry = entry.map_err(|_| DiffRefusal::Unreadable)?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." || name.eq_ignore_ascii_case(b".git") {
                    continue;
                }
                let mut path = prefix.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                let stat = statat(&parent, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|_| DiffRefusal::Unreadable)?;
                if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                    if tracked.contains(&path) {
                        continue;
                    }
                    if path.len() > dudes_proto::MAX_DIFF_PATH_BYTES {
                        cut = true;
                        continue;
                    }
                    let child = open_dir(&parent, entry.file_name())
                        .map_err(|_| DiffRefusal::Unreadable)?;
                    pending.push((child, path));
                } else if !tracked.contains(&path) {
                    count = count.saturating_add(1);
                }
            }
        }
        self.verify()?;
        Ok((count, cut))
    }
    #[cfg(test)]
    fn read(
        &self,
        path: &RepoPath,
        budget: &Budget,
    ) -> Result<Option<Vec<u8>>, DiffOmissionReason> {
        self.read_with(path, budget, || {})
    }
    #[cfg(test)]
    fn read_with(
        &self,
        path: &RepoPath,
        budget: &Budget,
        after_read: impl FnOnce(),
    ) -> Result<Option<Vec<u8>>, DiffOmissionReason> {
        self.sample_with(path, budget, after_read)
            .map(|sample| sample.map(|file| file.bytes))
    }
    fn sample_with(
        &self,
        path: &RepoPath,
        budget: &Budget,
        after_read: impl FnOnce(),
    ) -> Result<Option<CurrentFile>, DiffOmissionReason> {
        if super::text::sensitive_path(path.as_str()) {
            return Err(DiffOmissionReason::SensitivePath);
        }
        budget.check().map_err(|_| DiffOmissionReason::Unreadable)?;
        self.verify().map_err(|_| DiffOmissionReason::Raced)?;
        let mut parts = path.as_str().split('/').peekable();
        let mut parent = self
            .fd
            .try_clone()
            .map_err(|_| DiffOmissionReason::Unreadable)?;
        let mut parents = Vec::new();
        while let Some(part) = parts.next() {
            budget.check().map_err(|_| DiffOmissionReason::Unreadable)?;
            if parts.peek().is_some() {
                let child = match open_dir(&parent, part) {
                    Ok(child) => child,
                    Err(Errno::NOENT) => {
                        return self.absent(&parent, part, &parents, budget, after_read);
                    }
                    Err(_) => return Err(DiffOmissionReason::UnsupportedKind),
                };
                let id = identity(&child).map_err(|_| DiffOmissionReason::Unreadable)?;
                parents.push((parent, part.to_owned(), id));
                parent = child;
                continue;
            }
            let fd = match openat(
                &parent,
                part,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => {
                    return self.absent(&parent, part, &parents, budget, after_read);
                }
                Err(Errno::LOOP) => return Err(DiffOmissionReason::UnsupportedKind),
                Err(_) => return Err(DiffOmissionReason::Unreadable),
            };
            let mut file = File::from(fd);
            let initial = Stamp::file(&file).map_err(|_| DiffOmissionReason::Unreadable)?;
            if !initial.regular {
                return Err(DiffOmissionReason::UnsupportedKind);
            }
            if initial.size > MAX_DIFF_SIDE_BYTES as u64 {
                return Err(DiffOmissionReason::OversizeFile);
            }
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_DIFF_SIDE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| DiffOmissionReason::Unreadable)?;
            after_read();
            if bytes.len() > MAX_DIFF_SIDE_BYTES {
                return Err(DiffOmissionReason::OversizeFile);
            }
            let final_stamp = Stamp::file(&file).map_err(|_| DiffOmissionReason::Raced)?;
            let named = statat(&parent, part, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| DiffOmissionReason::Raced)?;
            if initial != final_stamp
                || bytes.len() as u64 != initial.size
                || FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
                || stat_identity(&named) != Some(initial.identity)
            {
                return Err(DiffOmissionReason::Raced);
            }
            self.verify_parents(&parents, budget)?;
            return Ok(Some(CurrentFile {
                bytes,
                mode: if initial.mode & 0o111 == 0 {
                    0o100_644
                } else {
                    0o100_755
                },
            }));
        }
        Err(DiffOmissionReason::Unreadable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn budget() -> Budget {
        Budget::new(Arc::new(AtomicBool::new(false)))
    }
    fn path(path: &str) -> RepoPath {
        RepoPath::from_bytes(path.as_bytes()).unwrap()
    }

    #[test]
    fn held_root_reads_regular_files_and_refuses_links_and_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("ok"), "hello\n").unwrap();
        std::fs::create_dir(tree.join("dir")).unwrap();
        symlink("ok", tree.join("linked")).unwrap();
        symlink(temp.path(), tree.join("parent")).unwrap();
        nix::unistd::mkfifo(&tree.join("pipe"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let budget = budget();
        let root = Root::open(&tree, None, &budget).unwrap();
        assert_eq!(
            root.read(&path("ok"), &budget),
            Ok(Some(b"hello\n".to_vec()))
        );
        assert_eq!(root.read(&path("missing"), &budget), Ok(None));
        for name in ["dir", "linked", "pipe", "parent/other"] {
            assert_eq!(
                root.read(&path(name), &budget),
                Err(DiffOmissionReason::UnsupportedKind),
                "{name}"
            );
        }
    }

    #[test]
    fn linked_and_replaced_roots_cannot_take_the_baselines_identity() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("ok"), "original").unwrap();
        let budget = budget();
        let root = Root::open(&tree, None, &budget).unwrap();
        let identity = root.identity();
        let alias = temp.path().join("alias");
        symlink(&tree, &alias).unwrap();
        assert!(matches!(
            Root::open(&alias, Some(identity), &budget),
            Err(DiffRefusal::RootMismatch)
        ));
        std::fs::rename(&tree, temp.path().join("old")).unwrap();
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("ok"), "outside").unwrap();
        assert!(matches!(
            Root::open(&tree, Some(identity), &budget),
            Err(DiffRefusal::RootMismatch)
        ));
        assert_eq!(
            root.read(&path("ok"), &budget),
            Err(DiffOmissionReason::Raced)
        );
    }

    #[test]
    fn changed_bytes_or_leaf_names_during_a_read_are_omitted_whole() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("ok"), "original").unwrap();
        let budget = budget();
        let root = Root::open(temp.path(), None, &budget).unwrap();
        assert_eq!(
            root.read_with(&path("ok"), &budget, || {
                std::fs::write(temp.path().join("ok"), "changed!").unwrap();
            }),
            Err(DiffOmissionReason::Raced)
        );
        assert_eq!(
            root.read_with(&path("ok"), &budget, || {
                std::fs::rename(temp.path().join("ok"), temp.path().join("old")).unwrap();
                std::fs::write(temp.path().join("ok"), "replaced").unwrap();
            }),
            Err(DiffOmissionReason::Raced)
        );
    }

    #[test]
    fn cancellation_and_expiry_refuse_before_opening_a_root() {
        let budget = Budget::new(Arc::new(AtomicBool::new(true)));
        assert!(matches!(
            Root::open(Path::new("/nonexistent"), None, &budget),
            Err(DiffRefusal::TimedOut)
        ));
    }

    #[test]
    fn a_missing_leaf_still_checks_root_identity_after_observation() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let budget = budget();
        let root = Root::open(&tree, None, &budget).unwrap();
        let observed = root.read_with(&path("missing"), &budget, || {
            std::fs::rename(&tree, temp.path().join("old")).unwrap();
            std::fs::create_dir(&tree).unwrap();
        });
        assert_eq!(observed, Err(DiffOmissionReason::Raced));
    }
}
