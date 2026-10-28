//! Directory-bound preparation and cleanup of copies that have never owned an agent.

use std::path::{Path, PathBuf};

use dudes_proto::record::WorktreeDirectoryAuthority;

use super::hire::LeaseView;

/// The restart location is derived solely from the durable lease, never a sidecar.
pub(crate) fn cleanup_path(lease: &LeaseView) -> Result<PathBuf, String> {
    let parent = lease.path.parent().ok_or("the copy has no parent")?;
    Ok(parent.join(format!(".pending-cleanup-{}", lease.worktree)))
}

/// Only a definite missing-path error proves absence.
pub(crate) fn absent(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
        Ok(_) => Ok(false),
    }
}

/// Recheck every derived location immediately before the absence record is appended.
pub(crate) fn settled(lease: &LeaseView) -> Result<(), String> {
    #[cfg(unix)]
    native::verify_absence(lease)?;
    let cleanup = cleanup_path(lease)?;
    let admin = lease
        .origin
        .join(".git/worktrees")
        .join(lease.path.file_name().ok_or("the copy has no name")?);
    for path in [&lease.path, &cleanup, &admin] {
        if !absent(path)? {
            return Err(format!(
                "{} still exists; pending cleanup is incomplete",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
mod native {
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::time::UNIX_EPOCH;

    use dudes_proto::record::WorktreeDirectoryIdentity;
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, RenameFlags};

    use super::{LeaseView, Path, WorktreeDirectoryAuthority, absent, cleanup_path};

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const TEXT_LIMIT: u64 = 4096;

    fn fail(error: impl std::fmt::Display) -> String {
        error.to_string()
    }

    fn open(path: &Path) -> Result<File, String> {
        rustix::fs::open(path, DIRECTORY, Mode::empty())
            .map(File::from)
            .map_err(fail)
    }

    fn child(parent: &File, name: impl rustix::path::Arg) -> Result<Option<File>, String> {
        match rustix::fs::openat(parent, name, DIRECTORY, Mode::empty()) {
            Ok(fd) => Ok(Some(File::from(fd))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(fail(error)),
        }
    }

    fn identity(file: &File) -> Result<WorktreeDirectoryIdentity, String> {
        let meta = file.metadata().map_err(fail)?;
        if !meta.is_dir() {
            return Err("the object is not a directory".into());
        }
        let created = meta
            .created()
            .map_err(|error| format!("directory birth identity is unavailable: {error}"))?
            .duration_since(UNIX_EPOCH)
            .map_err(fail)?;
        Ok(WorktreeDirectoryIdentity {
            device: meta.dev(),
            inode: meta.ino(),
            created_secs: created.as_secs(),
            created_nanos: created.subsec_nanos(),
        })
    }

    fn same(file: &File, expected: WorktreeDirectoryIdentity) -> Result<(), String> {
        if identity(file)? != expected {
            return Err("directory identity changed; retained for the operator".into());
        }
        Ok(())
    }

    fn checked(path: &Path, expected: WorktreeDirectoryIdentity) -> Result<File, String> {
        let file = open(path)?;
        same(&file, expected)?;
        Ok(file)
    }

    /// Exclusively create the empty root before the one durable required lease.
    pub(crate) fn prepare(
        origin: &Path,
        path: &Path,
    ) -> Result<WorktreeDirectoryAuthority, String> {
        let parent_path = path.parent().ok_or("the copy has no parent")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent_path)
            .map_err(fail)?;
        let parent = open(parent_path)?;
        let origin_file = open(origin)?;
        let common = open(&origin.join(".git"))?;
        let name = path.file_name().ok_or("the copy has no directory name")?;
        if !absent(&origin.join(".git/worktrees").join(name))? {
            return Err("the copy already has a Git registration".into());
        }
        let parent_identity = identity(&parent)?;
        let origin_identity = identity(&origin_file)?;
        let common_identity = identity(&common)?;
        rustix::fs::mkdirat(&parent, name, Mode::RWXU).map_err(fail)?;
        let root = child(&parent, name)?.ok_or("the newly created root disappeared")?;
        root.sync_all().map_err(fail)?;
        parent.sync_all().map_err(fail)?;
        Ok(WorktreeDirectoryAuthority {
            origin: origin_identity,
            common: common_identity,
            root: identity(&root)?,
            parent: parent_identity,
        })
    }

    fn authority(lease: &LeaseView) -> Result<(&WorktreeDirectoryAuthority, File, File), String> {
        let wanted = lease
            .identity
            .as_ref()
            .ok_or("legacy lease has no directory identity; retained for the operator")?;
        let parent = checked(
            lease.path.parent().ok_or("the copy has no parent")?,
            wanted.parent,
        )?;
        checked(&lease.origin, wanted.origin)?;
        let common = checked(&lease.origin.join(".git"), wanted.common)?;
        Ok((wanted, parent, common))
    }

    pub(super) fn verify_absence(lease: &LeaseView) -> Result<(), String> {
        let common = if lease.identity.is_some() {
            let (_, _, common) = authority(lease)?;
            common
        } else {
            open(&lease.origin.join(".git"))?
        };
        if let Some(worktrees) = child(&common, "worktrees")? {
            let name = lease.path.file_name().ok_or("the copy has no name")?;
            match rustix::fs::statat(&worktrees, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(fail(error)),
                Ok(_) => {
                    return Err("Git registration remains; pending cleanup is incomplete".into());
                }
            }
        }
        Ok(())
    }

    fn branch_at_base(lease: &LeaseView, common: &File) -> Result<(), String> {
        let refs = child(common, "refs")?.ok_or("Git refs are missing")?;
        let heads = child(&refs, "heads")?.ok_or("Git heads are missing")?;
        let branches = child(&heads, "dudes")?.ok_or("the pending branch is missing")?;
        if lease.branch != format!("dudes/{}", lease.worktree.short()) {
            return Err("the pending branch does not match its lease".into());
        }
        let value = text(&branches, lease.worktree.short())?
            .ok_or("the pending branch cannot be proved")?;
        if value.trim() != lease.base.to_string() {
            return Err(
                "the pending branch advanced or changed; its copy and ref are retained".into(),
            );
        }
        Ok(())
    }

    fn text(parent: &File, name: impl rustix::path::Arg) -> Result<Option<String>, String> {
        let fd = match rustix::fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(fail(error)),
        };
        let file = File::from(fd);
        if !file.metadata().map_err(fail)?.is_file() {
            return Err("Git metadata is not a regular file".into());
        }
        let mut value = String::new();
        file.take(TEXT_LIMIT + 1)
            .read_to_string(&mut value)
            .map_err(fail)?;
        if value.len() as u64 > TEXT_LIMIT {
            return Err("Git metadata is too large".into());
        }
        Ok(Some(value))
    }

    fn names(directory: &File) -> Result<Vec<std::ffi::CString>, String> {
        Dir::read_from(directory)
            .map_err(fail)?
            .filter_map(|item| match item {
                Ok(item) if item.file_name() == c"." || item.file_name() == c".." => None,
                Ok(item) => Some(Ok(item.file_name().to_owned())),
                Err(error) => Some(Err(fail(error))),
            })
            .collect()
    }

    fn validate_admin_entries(admin: &File) -> Result<(), String> {
        for item in names(admin)? {
            if !matches!(
                item.to_bytes(),
                b"HEAD"
                    | b"commondir"
                    | b"gitdir"
                    | b"index"
                    | b"index.lock"
                    | b"locked"
                    | b"logs"
                    | b"refs"
                    | b"ORIG_HEAD"
            ) {
                return Err(format!(
                    "unproved Git registration entry {}; retained",
                    item.to_string_lossy()
                ));
            }
        }
        if let Some(refs) = child(admin, "refs")?
            && !names(&refs)?.is_empty()
        {
            return Err("unproved worktree-specific refs; retained".into());
        }
        for name in ["index", "index.lock", "locked", "ORIG_HEAD"] {
            let metadata = rustix::fs::statat(admin, name, AtFlags::SYMLINK_NOFOLLOW);
            match metadata {
                Ok(meta) if rustix::fs::FileType::from_raw_mode(meta.st_mode).is_file() => {}
                Err(rustix::io::Errno::NOENT) => {}
                Ok(_) => return Err(format!("unproved Git registration entry {name}; retained")),
                Err(error) => return Err(fail(error)),
            }
        }
        if let Some(logs) = child(admin, "logs")? {
            for name in names(&logs)? {
                if name.to_bytes() != b"HEAD" {
                    return Err("unproved Git registration log; retained".into());
                }
                text(&logs, name.as_c_str())?;
            }
        }
        Ok(())
    }

    fn registration(
        lease: &LeaseView,
        root: Option<&File>,
        common: &File,
        complete: bool,
    ) -> Result<Option<(File, File)>, String> {
        let name = lease
            .path
            .file_name()
            .ok_or("the copy has no directory name")?;
        let admin_path = lease.origin.join(".git/worktrees").join(name);
        let gitfile = root.map(|root| text(root, ".git")).transpose()?.flatten();
        if let Some(value) = &gitfile {
            let target = value
                .trim()
                .strip_prefix("gitdir: ")
                .ok_or("the copy has an invalid gitfile")?;
            let target = Path::new(target);
            let target = if target.is_absolute() {
                target.to_path_buf()
            } else {
                lease.path.join(target)
            };
            if target != admin_path {
                return Err("the copy names an unexpected Git registration".into());
            }
        } else if complete {
            return Err("the copy has no gitfile".into());
        }
        if gitfile.is_some() {
            branch_at_base(lease, common)?;
        }
        let Some(worktrees) = child(common, "worktrees")? else {
            return if complete {
                Err("Git registration is missing".into())
            } else {
                Ok(None)
            };
        };
        let Some(admin) = child(&worktrees, name)? else {
            return if complete {
                Err("Git registration is missing".into())
            } else {
                Ok(None)
            };
        };
        validate_admin_entries(&admin)?;
        branch_at_base(lease, common)?;
        if let Some(value) = text(&admin, "ORIG_HEAD")?
            && value.trim() != lease.base.to_string()
        {
            return Err("Git registration retains another original HEAD; retained".into());
        }
        let backlink = text(&admin, "gitdir")?;
        let commondir = text(&admin, "commondir")?;
        let head = text(&admin, "HEAD")?;
        let canonical_parent =
            std::fs::canonicalize(lease.path.parent().ok_or("the copy has no parent")?)
                .map_err(fail)?;
        let canonical_gitfile = canonical_parent.join(name).join(".git");
        if let Some(value) = &backlink
            && Path::new(value.trim()) != canonical_gitfile
        {
            return Err("Git registration points to another root".into());
        }
        if let Some(value) = &commondir
            && value.trim() != "../.."
        {
            return Err("Git registration names an unproved common directory".into());
        }
        if let Some(value) = &head
            && value.trim() != format!("ref: refs/heads/{}", lease.branch)
        {
            return Err("Git registration names an unexpected branch".into());
        }
        if (complete && (backlink.is_none() || commondir.is_none() || head.is_none()))
            || (gitfile.is_none() && (backlink.is_none() || commondir.is_none()))
        {
            return Err("Git registration ownership is unproved; retained".into());
        }
        Ok(Some((worktrees, admin)))
    }

    /// Recheck the recorded empty root and origins before the owned Git child starts.
    pub(crate) fn verify_prepared(lease: &LeaseView) -> Result<(), String> {
        let (wanted, parent, _) = authority(lease)?;
        let root = child(
            &parent,
            lease.path.file_name().ok_or("the copy has no name")?,
        )?
        .ok_or("the copy is missing")?;
        same(&root, wanted.root)?;
        if !names(&root)?.is_empty() {
            return Err("the prepared copy is no longer empty".into());
        }
        Ok(())
    }

    /// Verify the durable roots and complete Git links before final assignment.
    pub(crate) fn verify(lease: &LeaseView) -> Result<(), String> {
        let (wanted, parent, common) = authority(lease)?;
        let root = child(
            &parent,
            lease.path.file_name().ok_or("the copy has no name")?,
        )?
        .ok_or("the copy is missing")?;
        same(&root, wanted.root)?;
        registration(lease, Some(&root), &common, true)?;
        Ok(())
    }

    fn remove_contents(directory: &File) -> Result<(), String> {
        for name in names(directory)? {
            match rustix::fs::openat(directory, name.as_c_str(), DIRECTORY, Mode::empty()) {
                Ok(fd) => {
                    remove_contents(&File::from(fd))?;
                    rustix::fs::unlinkat(directory, name.as_c_str(), AtFlags::REMOVEDIR)
                        .map_err(fail)?;
                }
                Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                    rustix::fs::unlinkat(directory, name.as_c_str(), AtFlags::empty())
                        .map_err(fail)?;
                }
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(fail(error)),
            }
        }
        Ok(())
    }

    fn remove_bound(
        parent: &File,
        name: impl rustix::path::Arg + Copy,
        directory: &File,
    ) -> Result<(), String> {
        remove_contents(directory)?;
        let current = child(parent, name)?.ok_or("cleanup directory moved during removal")?;
        same(&current, identity(directory)?)?;
        rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(fail)
    }

    pub(crate) fn discard(lease: &LeaseView) -> Result<(), String> {
        discard_with(lease, || {}, || {})
    }

    fn discard_with(
        lease: &LeaseView,
        before_move: impl FnOnce(),
        after_move: impl FnOnce(),
    ) -> Result<(), String> {
        let cleanup = cleanup_path(lease)?;
        if lease.identity.is_none() {
            if !absent(&lease.path)? || !absent(&cleanup)? {
                return Err(
                    "legacy lease has no directory identity; retained for the operator".into(),
                );
            }
            let admin = lease
                .origin
                .join(".git/worktrees")
                .join(lease.path.file_name().ok_or("the copy has no name")?);
            return if absent(&admin)? {
                Ok(())
            } else {
                Err("legacy Git registration is unproved; retained".into())
            };
        }
        let (wanted, parent, common) = authority(lease)?;
        let name = lease.path.file_name().ok_or("the copy has no name")?;
        let holding = cleanup.file_name().ok_or("cleanup has no name")?;
        let original = child(&parent, name)?;
        let held = child(&parent, holding)?;
        let root = match (original, held) {
            (None, None) => {
                if registration(lease, None, &common, false)?.is_some() {
                    return Err("the absent root has an unproved Git registration; retained".into());
                }
                return Ok(());
            }
            (Some(_), Some(_)) => {
                return Err("both the leased root and cleanup location exist; retained".into());
            }
            (None, Some(root)) => {
                same(&root, wanted.root)?;
                root
            }
            (Some(root), None) => {
                same(&root, wanted.root)?;
                registration(lease, Some(&root), &common, false)?;
                before_move();
                rustix::fs::renameat_with(&parent, name, &parent, holding, RenameFlags::NOREPLACE)
                    .map_err(fail)?;
                parent.sync_all().map_err(fail)?;
                let moved = child(&parent, holding)?.ok_or("the moved root disappeared")?;
                same(&moved, wanted.root)?;
                after_move();
                moved
            }
        };
        same(&root, wanted.root)?;
        if let Some((worktrees, admin)) = registration(lease, Some(&root), &common, false)? {
            remove_bound(&worktrees, name, &admin)?;
            worktrees.sync_all().map_err(fail)?;
        }
        remove_bound(&parent, holding, &root)?;
        parent.sync_all().map_err(fail)?;
        checked(
            lease.path.parent().ok_or("the copy has no parent")?,
            wanted.parent,
        )?;
        if !absent(&lease.path)? || !absent(&cleanup)? {
            return Err("a copy or cleanup root remains; retained".into());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(unix)]
pub(crate) use native::{discard, prepare, verify, verify_prepared};

#[cfg(not(unix))]
pub(crate) fn prepare(_: &Path, _: &Path) -> Result<WorktreeDirectoryAuthority, String> {
    Err("directory-bound worktree creation is unsupported on this platform".into())
}

#[cfg(not(unix))]
pub(crate) fn verify(_: &LeaseView) -> Result<(), String> {
    Err("directory identity is unsupported on this platform".into())
}

#[cfg(not(unix))]
pub(crate) fn discard(_: &LeaseView) -> Result<(), String> {
    Err("directory-bound cleanup is unsupported on this platform; retained".into())
}

#[cfg(not(unix))]
pub(crate) fn verify_prepared(_: &LeaseView) -> Result<(), String> {
    Err("directory identity is unsupported on this platform".into())
}
