use std::fs::{File, Metadata};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use dudes_proto::record::FileIdentity;
use dudes_providers::scoped::{ScopedInventory, WritableLocation, WritablePurpose};

use super::super::{normalized_absolute, refusal};

const ENTRY_LIMIT: usize = 32_768;

pub(super) struct Inspection {
    inventory: ScopedInventory,
    paths: Vec<BoundPath>,
}

struct BoundPath {
    path: PathBuf,
    file: File,
}

impl Inspection {
    pub(super) fn capture(inventory: &ScopedInventory, deadline: Instant) -> io::Result<Self> {
        validate_inventory(inventory)?;
        let mut paths = Vec::new();
        for root in inventory
            .writable_roots()
            .iter()
            .chain(&inventory.protected)
        {
            paths.push(BoundPath::open(root)?);
        }
        for entry in inventory.hired.repositories.entries() {
            let repository = &entry.value;
            for (path, expected) in [
                (
                    &repository.origin.directory,
                    repository.origin.directory_identity,
                ),
                (
                    &repository.origin.common_dir,
                    repository.origin.common_identity,
                ),
                (
                    &repository.execution.directory,
                    repository.execution.directory_identity,
                ),
                (
                    &repository.execution.git_dir,
                    repository.execution.git_identity,
                ),
            ] {
                let bound = BoundPath::open(path)?;
                if identity(&bound.file.metadata()?)? != expected {
                    return Err(refusal(
                        "scoped repository identity differs from the frozen hire",
                    ));
                }
                paths.push(bound);
            }
        }
        let inspection = Self {
            inventory: inventory.clone(),
            paths,
        };
        inspection.check(deadline)?;
        Ok(inspection)
    }

    pub(super) fn check(&self, deadline: Instant) -> io::Result<()> {
        self.check_roots()?;
        let mut entries = 0;
        for path in self.inventory.writable_roots() {
            inspect_tree(
                &path,
                &self.inventory.protected,
                deadline,
                &mut entries,
                ENTRY_LIMIT,
            )?;
        }
        Ok(())
    }

    pub(super) fn check_roots(&self) -> io::Result<()> {
        validate_inventory(&self.inventory)?;
        for bound in &self.paths {
            let now = BoundPath::open(&bound.path)?;
            if !same(&now.file.metadata()?, &bound.file.metadata()?) {
                return Err(refusal("scoped root identity changed"));
            }
        }
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the complete frozen writable and protected union is checked in one place"
)]
fn validate_inventory(inventory: &ScopedInventory) -> io::Result<()> {
    let grant = &inventory.grant;
    let hired = &inventory.hired;
    if grant.hired_lsn == dudes_types::Lsn::ZERO
        || grant.catalog_lsn != hired.catalog_lsn
        || grant.project != hired.project.id
        || grant.policy != hired.policy
        || inventory.cwd != grant.repositories.primary().execution.directory
        || grant
            .repositories
            .entries()
            .iter()
            .any(|entry| hired.repositories.get(&entry.repo_id) != Some(&entry.value))
    {
        return Err(refusal(
            "scoped inventory differs from the frozen authority",
        ));
    }
    let p = &inventory.private;
    let agent = p
        .outbox
        .parent()
        .ok_or_else(|| refusal("scoped agent path is unavailable"))?;
    if p.work != agent.join("work") || p.temp != agent.join("tmp") || p.home != agent.join("home") {
        return Err(refusal("scoped private paths differ from the agent owner"));
    }
    let mut writable: Vec<_> = grant
        .repositories
        .entries()
        .iter()
        .map(|entry| WritableLocation {
            purpose: WritablePurpose::Repository(entry.repo_id.clone()),
            path: entry.value.execution.directory.clone(),
        })
        .collect();
    writable.extend([
        WritableLocation {
            purpose: WritablePurpose::Work,
            path: p.work.clone(),
        },
        WritableLocation {
            purpose: WritablePurpose::Outbox,
            path: p.outbox.clone(),
        },
        WritableLocation {
            purpose: WritablePurpose::Temporary,
            path: p.temp.clone(),
        },
    ]);
    if inventory.writable != writable {
        return Err(refusal(
            "scoped writable inventory was widened or reordered",
        ));
    }
    let mut protected = Vec::new();
    for entry in hired.repositories.entries() {
        let repository = &entry.value;
        let origin_git = git_target(&repository.origin.directory.join(".git"))?;
        if git_target(&repository.execution.directory.join(".git"))? != repository.execution.git_dir
        {
            return Err(refusal("scoped execution Git metadata changed"));
        }
        protected.extend([
            repository.origin.directory.join(".git"),
            repository.execution.directory.join(".git"),
            repository.execution.git_dir.clone(),
            origin_git,
            repository.origin.common_dir.clone(),
        ]);
        for private in [
            &p.work,
            &p.outbox,
            &p.temp,
            &p.home,
            &p.profile,
            &agent.join("inbox"),
        ] {
            if overlaps(private, &repository.origin.directory)
                || overlaps(private, &repository.execution.directory)
                || overlaps(private, &repository.origin.common_dir)
                || overlaps(private, &repository.execution.git_dir)
            {
                return Err(refusal("scoped private paths overlap repository authority"));
            }
        }
    }
    protected.extend([p.home.clone(), p.profile.clone(), agent.join("inbox")]);
    let mut seen = std::collections::BTreeSet::new();
    protected.retain(|root| seen.insert(root.clone()));
    if inventory.protected != protected {
        return Err(refusal("scoped protected inventory changed"));
    }
    for (index, root) in writable.iter().enumerate() {
        if writable[..index]
            .iter()
            .any(|other| overlaps(&root.path, &other.path))
            || [&p.home, &p.profile, &agent.join("inbox")]
                .iter()
                .any(|control| overlaps(&root.path, control))
        {
            return Err(refusal("scoped roots overlap protected control paths"));
        }
    }
    Ok(())
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn git_target(path: &Path) -> io::Result<PathBuf> {
    let file = open(path)?;
    if file.metadata()?.is_dir() {
        return path.canonicalize();
    }
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    if text.len() > 4096 {
        return Err(refusal("scoped Git metadata is incomplete"));
    }
    let value = text
        .strip_prefix("gitdir: ")
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.contains(['\n', '\r']))
        .ok_or_else(|| refusal("scoped Git metadata is invalid"))?;
    let target = Path::new(value);
    let target = if target.is_absolute() {
        target.to_owned()
    } else {
        path.parent()
            .ok_or_else(|| refusal("scoped Git parent is unavailable"))?
            .join(target)
    };
    target.canonicalize()
}

impl BoundPath {
    fn open(path: &Path) -> io::Result<Self> {
        normalized_absolute(path)?;
        if path.canonicalize()? != path {
            return Err(refusal("scoped root is aliased"));
        }
        Ok(Self {
            path: path.to_owned(),
            file: open(path)?,
        })
    }
}

#[cfg(unix)]
fn open(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let file: File = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?
    .into();
    let metadata = file.metadata()?;
    if (!metadata.is_dir() && !metadata.is_file())
        || !same(&metadata, &std::fs::symlink_metadata(path)?)
    {
        return Err(refusal("scoped root has an unsupported or changing type"));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open(_path: &Path) -> io::Result<File> {
    Err(refusal(
        "scoped root inspection is unavailable on this platform",
    ))
}

pub(super) fn open_file(path: &Path) -> io::Result<File> {
    let file = BoundPath::open(path)?.file;
    let metadata = file.metadata()?;
    if !metadata.is_file() || multiply_linked(&metadata) {
        return Err(refusal(
            "scoped pinned file is aliased or is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
pub(super) fn same(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type() == right.file_type()
}

#[cfg(not(unix))]
pub(super) fn same(_left: &Metadata, _right: &Metadata) -> bool {
    false
}

#[cfg(unix)]
pub(super) fn changed(metadata: &Metadata) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt as _;
    (metadata.ctime(), metadata.ctime_nsec())
}

#[cfg(not(unix))]
pub(super) fn changed(_metadata: &Metadata) -> (i64, i64) {
    (0, 0)
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "other platforms refuse identity inspection through this fallible interface"
)]
fn identity(metadata: &Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn identity(_metadata: &Metadata) -> io::Result<FileIdentity> {
    Err(refusal("scoped root identity is unavailable"))
}

#[cfg(unix)]
fn multiply_linked(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink() != 1
}

#[cfg(not(unix))]
fn multiply_linked(_metadata: &Metadata) -> bool {
    true
}

#[cfg(unix)]
fn inspect_tree(
    path: &Path,
    protected: &[PathBuf],
    deadline: Instant,
    count: &mut usize,
    limit: usize,
) -> io::Result<()> {
    use rustix::fs::{Dir, Mode, OFlags};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let first = BoundPath::open(path)?;
    let device = identity(&first.file.metadata()?)?.device;
    let mut pending = vec![(path.to_owned(), first.file)];
    while let Some((path, directory)) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(refusal("scoped root inspection deadline expired"));
        }
        for entry in Dir::read_from(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            *count += 1;
            if *count > limit || Instant::now() >= deadline {
                return Err(refusal("scoped root inspection is incomplete"));
            }
            let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
            if protected.iter().any(|root| child_path.starts_with(root)) {
                continue;
            }
            let file: File = rustix::fs::openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )?
            .into();
            let metadata = file.metadata()?;
            if identity(&metadata)?.device != device
                || !same(&metadata, &std::fs::symlink_metadata(&child_path)?)
            {
                return Err(refusal(
                    "scoped root crosses a changing or mounted directory",
                ));
            }
            if metadata.is_dir() {
                pending.push((child_path, file));
            } else if !metadata.is_file() || multiply_linked(&metadata) {
                return Err(refusal("scoped writable file is aliased or unsupported"));
            }
        }
        if !same(&directory.metadata()?, &std::fs::symlink_metadata(&path)?) {
            return Err(refusal("scoped directory changed during inspection"));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn inspect_tree(
    _path: &Path,
    _protected: &[PathBuf],
    _deadline: Instant,
    _count: &mut usize,
    _limit: usize,
) -> io::Result<()> {
    Err(refusal("scoped writable inspection is unavailable"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn writable_aliases_and_incomplete_inspection_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        std::fs::write(path.join("file"), b"data").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(inspect_tree(&path, &[], deadline, &mut 0, 0).is_err());
        assert!(inspect_tree(&path, &[], Instant::now(), &mut 0, 100).is_err());
        std::fs::hard_link(path.join("file"), path.join("alias")).unwrap();
        assert!(inspect_tree(&path, &[], deadline, &mut 0, 100).is_err());
        std::fs::remove_file(path.join("alias")).unwrap();
        std::os::unix::fs::symlink(path.join("file"), path.join("alias")).unwrap();
        assert!(inspect_tree(&path, &[], deadline, &mut 0, 100).is_err());
    }
}
