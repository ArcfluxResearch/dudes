use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dudes_proto::DiffRefusal;
use dudes_types::GitCommit;
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fstat, linkat, mkdirat, openat, statat, unlinkat,
};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use super::RootIdentity;
use super::files::{Budget, Root, Stamp, identity, open_dir};
use crate::git_read::shadow::{
    CONFIG, HEAD_MAX, INDEX_MAX, LINK_MAX, LOOSE_REF_MAX, PACKED_REFS_MAX, SHALLOW_MAX,
};
use crate::git_read::{Tree, parse};

const MARKER: &str = ".dudes-content-shadow";

struct Binding {
    parent: Arc<File>,
    name: OsString,
    file: Arc<File>,
    stamp: Stamp,
    kind: FileType,
}

impl Binding {
    fn new(
        parent: Arc<File>,
        name: impl Into<OsString>,
        file: Arc<File>,
    ) -> Result<Self, DiffRefusal> {
        let stamp = Stamp::file(&file)?;
        let stat = fstat(file.as_ref()).map_err(|_| DiffRefusal::Unreadable)?;
        let binding = Self {
            parent,
            name: name.into(),
            file,
            stamp,
            kind: FileType::from_raw_mode(stat.st_mode),
        };
        binding.verify()?;
        Ok(binding)
    }

    fn verify(&self) -> Result<(), DiffRefusal> {
        if Stamp::file(&self.file)? != self.stamp {
            return Err(DiffRefusal::Unreadable);
        }
        verify_name(&self.parent, &self.name, &self.file, self.kind)
    }
}

fn verify_name(
    parent: &File,
    name: &OsStr,
    file: &File,
    kind: FileType,
) -> Result<(), DiffRefusal> {
    let named =
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| DiffRefusal::Unreadable)?;
    let held = fstat(file).map_err(|_| DiffRefusal::Unreadable)?;
    if FileType::from_raw_mode(named.st_mode) != kind
        || (named.st_dev, named.st_ino) != (held.st_dev, held.st_ino)
    {
        return Err(DiffRefusal::Unreadable);
    }
    Ok(())
}

fn bound_dir(
    parent: &Arc<File>,
    name: &OsStr,
    bindings: &mut Vec<Binding>,
) -> Result<Arc<File>, DiffRefusal> {
    let file = Arc::new(File::from(
        open_dir(parent.as_ref(), name).map_err(|_| DiffRefusal::UnsupportedStorage)?,
    ));
    bindings.push(Binding::new(parent.clone(), name, file.clone())?);
    Ok(file)
}

fn optional_dir(
    parent: &Arc<File>,
    name: &OsStr,
    bindings: &mut Vec<Binding>,
) -> Result<Option<Arc<File>>, DiffRefusal> {
    match statat(parent.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => bound_dir(parent, name, bindings).map(Some),
        Err(Errno::NOENT) => Ok(None),
        Err(_) => Err(DiffRefusal::Unreadable),
    }
}

fn open_regular(parent: &File, name: &OsStr) -> Result<File, DiffRefusal> {
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| DiffRefusal::UnsupportedStorage)?;
    let file = File::from(fd);
    if !file
        .metadata()
        .map_err(|_| DiffRefusal::Unreadable)?
        .is_file()
    {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    Ok(file)
}

fn read_small(
    parent: &Arc<File>,
    name: &str,
    limit: u64,
    bindings: &mut Vec<Binding>,
    budget: &Budget,
) -> Result<Option<Vec<u8>>, DiffRefusal> {
    budget.check()?;
    if matches!(
        statat(parent.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW),
        Err(Errno::NOENT)
    ) {
        return Ok(None);
    }
    let file = Arc::new(open_regular(parent, OsStr::new(name))?);
    if file.metadata().map_err(|_| DiffRefusal::Unreadable)?.len() > limit {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let binding = Binding::new(parent.clone(), name, file.clone())?;
    let mut bytes = Vec::new();
    let mut input = file.as_ref().take(limit + 1);
    let mut chunk = [0; 8192];
    loop {
        budget.check()?;
        let count = input
            .read(&mut chunk)
            .map_err(|_| DiffRefusal::Unreadable)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    if bytes.len() as u64 > limit {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    binding.verify()?;
    bindings.push(binding);
    Ok(Some(bytes))
}

fn absent(parent: &File, name: &str) -> Result<(), DiffRefusal> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Ok(_) => Err(DiffRefusal::UnsupportedStorage),
        Err(_) => Err(DiffRefusal::Unreadable),
    }
}

fn verify_link_target_identity(
    parent: &Arc<File>,
    name: &str,
    prefix: &[u8],
    target: &File,
    bindings: &mut Vec<Binding>,
    budget: &Budget,
) -> Result<(), DiffRefusal> {
    let bytes = read_small(parent, name, LINK_MAX, bindings, budget)?
        .ok_or(DiffRefusal::WorktreeMismatch)?;
    let path = bytes
        .strip_prefix(prefix)
        .ok_or(DiffRefusal::WorktreeMismatch)?;
    let path = path.strip_suffix(b"\n").unwrap_or(path);
    if path.is_empty() || path.iter().any(|byte| matches!(byte, 0 | b'\n' | b'\r')) {
        return Err(DiffRefusal::WorktreeMismatch);
    }
    let named = statat(parent.as_ref(), OsStr::from_bytes(path), AtFlags::empty())
        .map_err(|_| DiffRefusal::WorktreeMismatch)?;
    let held = fstat(target).map_err(|_| DiffRefusal::WorktreeMismatch)?;
    if FileType::from_raw_mode(named.st_mode) != FileType::Directory
        || (named.st_dev, named.st_ino) != (held.st_dev, held.st_ino)
    {
        return Err(DiffRefusal::WorktreeMismatch);
    }
    Ok(())
}

fn packed_head(
    bytes: &[u8],
    wanted: &str,
    budget: &Budget,
) -> Result<Option<GitCommit>, DiffRefusal> {
    let mut found = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        budget.check()?;
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(peeled) = line.strip_prefix(b"^") {
            if !parse::is_object_id(peeled) {
                return Err(DiffRefusal::UnsupportedStorage);
            }
            continue;
        }
        let (id, name) = line
            .split_at_checked(40)
            .ok_or(DiffRefusal::UnsupportedStorage)?;
        let name = name
            .strip_prefix(b" ")
            .and_then(|name| std::str::from_utf8(name).ok())
            .ok_or(DiffRefusal::UnsupportedStorage)?;
        if !parse::is_object_id(id)
            || !name.starts_with("refs/")
            || !parse::is_legal_ref_name(name)
            || name.starts_with("refs/replace/")
        {
            return Err(DiffRefusal::UnsupportedStorage);
        }
        if name == wanted {
            if found.is_some() {
                return Err(DiffRefusal::UnsupportedStorage);
            }
            found = Some(parse_commit(id)?);
        }
    }
    Ok(found)
}

fn parse_commit(bytes: &[u8]) -> Result<GitCommit, DiffRefusal> {
    GitCommit::parse(std::str::from_utf8(bytes).map_err(|_| DiffRefusal::Unreadable)?)
        .map_err(|_| DiffRefusal::Unreadable)
}

fn resolve_head(
    own: &Arc<File>,
    common: &Arc<File>,
    bindings: &mut Vec<Binding>,
    budget: &Budget,
) -> Result<GitCommit, DiffRefusal> {
    let bytes =
        read_small(own, "HEAD", HEAD_MAX, bindings, budget)?.ok_or(DiffRefusal::Unreadable)?;
    let head = parse::head(&bytes).ok_or(DiffRefusal::Unreadable)?;
    let packed = read_small(common, "packed-refs", PACKED_REFS_MAX, bindings, budget)?;
    match head {
        parse::Head::Detached(id) => {
            if let Some(packed) = packed {
                let _ = packed_head(&packed, "", budget)?;
            }
            parse_commit(&id)
        }
        parse::Head::Branch(name) => {
            let reference = format!("refs/heads/{name}");
            let packed = packed
                .map(|bytes| packed_head(&bytes, &reference, budget))
                .transpose()?
                .flatten();
            let mut parent = common.clone();
            let mut parts = reference.split('/').peekable();
            while let Some(part) = parts.next() {
                budget.check()?;
                if parts.peek().is_none() {
                    if let Some(bytes) = read_small(&parent, part, LOOSE_REF_MAX, bindings, budget)?
                    {
                        return parse_commit(bytes.strip_suffix(b"\n").unwrap_or(&bytes));
                    }
                } else if let Some(next) = optional_dir(&parent, OsStr::new(part), bindings)? {
                    parent = next;
                } else {
                    break;
                }
            }
            packed.ok_or(DiffRefusal::BaselineUnavailable)
        }
    }
}

struct OwnedName {
    parent: Arc<File>,
    name: OsString,
    file: Arc<File>,
    kind: FileType,
}

struct OwnedLayout {
    parent: Root,
    name: String,
    root: Arc<File>,
    entries: Vec<OwnedName>,
    copied_stamps: Vec<(Arc<File>, Stamp)>,
    budget: Budget,
}

impl OwnedLayout {
    fn create(scratch: &Path, name: &str, budget: &Budget) -> Result<Self, DiffRefusal> {
        budget.check()?;
        if !single_component(name.as_bytes()) {
            return Err(DiffRefusal::Unreadable);
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(scratch)
            .map_err(|_| DiffRefusal::Unreadable)?;
        let parent = Root::open(scratch, None, budget)?;
        private_dir(parent.fd())?;
        mkdirat(parent.fd(), name, Mode::RWXU).map_err(|_| DiffRefusal::Unreadable)?;
        let root = Arc::new(File::from(
            open_dir(parent.fd(), name).map_err(|_| DiffRefusal::Unreadable)?,
        ));
        verify_name(
            &File::from(
                parent
                    .fd()
                    .try_clone()
                    .map_err(|_| DiffRefusal::Unreadable)?,
            ),
            OsStr::new(name),
            &root,
            FileType::Directory,
        )?;
        private_dir(root.as_ref())?;
        let mut layout = Self {
            parent,
            name: name.to_owned(),
            root,
            entries: Vec::new(),
            copied_stamps: Vec::new(),
            budget: budget.clone(),
        };
        let marker = marker_bytes(identity(layout.root.as_ref())?);
        layout.put(&layout.root.clone(), MARKER, &marker)?;
        Ok(layout)
    }

    fn directory(&mut self, parent: &Arc<File>, name: &str) -> Result<Arc<File>, DiffRefusal> {
        self.budget.check()?;
        mkdirat(parent.as_ref(), name, Mode::RWXU).map_err(|_| DiffRefusal::Unreadable)?;
        let file = Arc::new(File::from(
            open_dir(parent.as_ref(), name).map_err(|_| DiffRefusal::Unreadable)?,
        ));
        self.entries.push(OwnedName {
            parent: parent.clone(),
            name: name.into(),
            file: file.clone(),
            kind: FileType::Directory,
        });
        self.record(
            parent,
            OsStr::new(name),
            identity(file.as_ref())?,
            FileType::Directory,
        )?;
        Ok(file)
    }

    fn put(&mut self, parent: &Arc<File>, name: &str, bytes: &[u8]) -> Result<(), DiffRefusal> {
        self.budget.check()?;
        let fd = openat(
            parent.as_ref(),
            name,
            OFlags::CREATE
                | OFlags::EXCL
                | OFlags::RDWR
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| DiffRefusal::Unreadable)?;
        let mut file = File::from(fd);
        let held = Arc::new(file.try_clone().map_err(|_| DiffRefusal::Unreadable)?);
        self.entries.push(OwnedName {
            parent: parent.clone(),
            name: name.into(),
            file: held.clone(),
            kind: FileType::RegularFile,
        });
        if name != MARKER {
            self.record(
                parent,
                OsStr::new(name),
                identity(held.as_ref())?,
                FileType::RegularFile,
            )?;
        }
        for chunk in bytes.chunks(8192) {
            self.budget.check()?;
            file.write_all(chunk).map_err(|_| DiffRefusal::Unreadable)?;
        }
        self.copied_stamps.push((held, Stamp::file(&file)?));
        Ok(())
    }

    fn child_path(&self, parent: &File, name: &OsStr) -> Result<String, DiffRefusal> {
        let name = name.to_str().ok_or(DiffRefusal::Unreadable)?;
        let id = identity(parent)?;
        if id == identity(self.root.as_ref())? {
            return Ok(name.to_owned());
        }
        let directory = self
            .entries
            .iter()
            .find(|entry| {
                entry.kind == FileType::Directory && identity(entry.file.as_ref()).ok() == Some(id)
            })
            .ok_or(DiffRefusal::Unreadable)?;
        Ok(format!(
            "{}/{name}",
            self.child_path(&directory.parent, &directory.name)?
        ))
    }

    fn record(
        &mut self,
        parent: &File,
        name: &OsStr,
        id: RootIdentity,
        kind: FileType,
    ) -> Result<(), DiffRefusal> {
        self.budget.check()?;
        let path = self.child_path(parent, name)?;
        let kind = match kind {
            FileType::Directory => 'd',
            FileType::RegularFile => 'f',
            _ => return Err(DiffRefusal::Unreadable),
        };
        let line = format!("{kind} {} {} {path}\n", id.device, id.inode);
        let marker = self
            .entries
            .iter()
            .find(|entry| entry.name == MARKER)
            .ok_or(DiffRefusal::Unreadable)?
            .file
            .clone();
        verify_name(
            &self.root,
            OsStr::new(MARKER),
            &marker,
            FileType::RegularFile,
        )?;
        let mut output = marker.as_ref();
        output
            .seek(SeekFrom::End(0))
            .and_then(|_| output.write_all(line.as_bytes()))
            .map_err(|_| DiffRefusal::Unreadable)?;
        let (_, stamp) = self
            .copied_stamps
            .iter_mut()
            .find(|(file, _)| Arc::ptr_eq(file, &marker))
            .ok_or(DiffRefusal::Unreadable)?;
        *stamp = Stamp::file(&marker)?;
        Ok(())
    }

    fn verify(&self) -> Result<(), DiffRefusal> {
        self.parent.verify()?;
        let parent = File::from(
            self.parent
                .fd()
                .try_clone()
                .map_err(|_| DiffRefusal::Unreadable)?,
        );
        verify_name(
            &parent,
            OsStr::new(&self.name),
            &self.root,
            FileType::Directory,
        )?;
        let root_id = identity(self.root.as_ref())?;
        let mut directories = BTreeMap::from([(
            (root_id.device, root_id.inode),
            (self.root.clone(), Vec::new()),
        )]);
        for entry in &self.entries {
            self.budget.check()?;
            verify_name(&entry.parent, &entry.name, &entry.file, entry.kind)?;
            let parent_id = identity(entry.parent.as_ref())?;
            directories
                .entry((parent_id.device, parent_id.inode))
                .or_insert_with(|| (entry.parent.clone(), Vec::new()))
                .1
                .push(entry.name.clone());
            if entry.kind == FileType::Directory {
                let id = identity(entry.file.as_ref())?;
                directories
                    .entry((id.device, id.inode))
                    .or_insert_with(|| (entry.file.clone(), Vec::new()));
            }
        }
        for (_, (directory, mut expected)) in directories {
            self.budget.check()?;
            expected.sort();
            if names(&directory, &self.budget)? != expected {
                return Err(DiffRefusal::Unreadable);
            }
        }
        for (file, stamp) in &self.copied_stamps {
            self.budget.check()?;
            if Stamp::file(file)? != *stamp {
                return Err(DiffRefusal::Unreadable);
            }
        }
        Ok(())
    }

    fn disarm_recovery(&self) {
        if self.budget.check().is_err() || self.parent.verify().is_err() {
            return;
        }
        let Ok(parent_fd) = self.parent.fd().try_clone() else {
            return;
        };
        if verify_name(
            &File::from(parent_fd),
            OsStr::new(&self.name),
            &self.root,
            FileType::Directory,
        )
        .is_err()
        {
            return;
        }
        if let Some(marker) = self.entries.iter().find(|entry| entry.name == MARKER)
            && verify_name(
                &marker.parent,
                &marker.name,
                &marker.file,
                FileType::RegularFile,
            )
            .is_ok()
        {
            let _ = unlinkat(marker.parent.as_ref(), &marker.name, AtFlags::empty());
        }
    }

    fn cleanup(&self) {
        if self.budget.check().is_err() || self.parent.verify().is_err() {
            return;
        }
        let Ok(parent_fd) = self.parent.fd().try_clone() else {
            return;
        };
        let parent = File::from(parent_fd);
        if verify_name(
            &parent,
            OsStr::new(&self.name),
            &self.root,
            FileType::Directory,
        )
        .is_err()
        {
            return;
        }
        if self.verify().is_err() {
            self.disarm_recovery();
        }
        for entry in self.entries.iter().rev() {
            if self.budget.check().is_err() {
                return;
            }
            if entry.name == MARKER
                && identity(entry.parent.as_ref()).ok() == identity(self.root.as_ref()).ok()
                && names(&self.root, &self.budget).ok().as_deref()
                    != Some(&[OsString::from(MARKER)])
            {
                continue;
            }
            if verify_name(&entry.parent, &entry.name, &entry.file, entry.kind).is_ok() {
                let flags = if entry.kind == FileType::Directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                };
                // Residual: a name can be swapped between the check above and this unlink. The
                // unlink runs against the held, identity-verified private directory, so it removes
                // at most one entry there, follows no link, and never touches content linked
                // elsewhere. Anything able to insert an entry here already runs as this user.
                let removed = unlinkat(entry.parent.as_ref(), &entry.name, flags);
                #[cfg(test)]
                if removed.is_ok() {
                    process_tests::after_unlink(entry);
                }
                let _ = removed;
            }
        }
        if self.budget.check().is_ok()
            && verify_name(
                &parent,
                OsStr::new(&self.name),
                &self.root,
                FileType::Directory,
            )
            .is_ok()
        {
            // POSIX cannot atomically unlink a directory only if its inode still matches.
            let _ = unlinkat(&parent, self.name.as_str(), AtFlags::REMOVEDIR);
        }
    }
}

impl Drop for OwnedLayout {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn recover_layout(
    scratch: &Path,
    parent_identity: RootIdentity,
    name: &str,
    budget: &Budget,
) -> Result<OwnedLayout, DiffRefusal> {
    if !single_component(name.as_bytes()) {
        return Err(DiffRefusal::Unreadable);
    }
    let parent = Root::open(scratch, Some(parent_identity), budget)?;
    let root = Arc::new(File::from(
        open_dir(parent.fd(), name).map_err(|_| DiffRefusal::Unreadable)?,
    ));
    private_dir(root.as_ref())?;
    let root_stamp = Stamp::file(&root)?;
    let marker = Arc::new(open_regular(&root, OsStr::new(MARKER))?);
    let meta = marker.metadata().map_err(|_| DiffRefusal::Unreadable)?;
    let expected = marker_bytes(identity(root.as_ref())?);
    if meta.uid() != nix::unistd::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() < expected.len() as u64
    {
        return Err(DiffRefusal::Unreadable);
    }
    let marker_stamp = Stamp::file(&marker)?;
    let ownership = read_ownership(&marker, &expected, budget)?;
    if Stamp::file(&marker)? != marker_stamp {
        return Err(DiffRefusal::Unreadable);
    }
    let mut entries = vec![OwnedName {
        parent: root.clone(),
        name: MARKER.into(),
        file: marker.clone(),
        kind: FileType::RegularFile,
    }];
    for entry in names(&root, budget)? {
        budget.check()?;
        match entry.to_str() {
            Some(MARKER) => {}
            Some("config" | "HEAD" | "index" | "shallow") => {
                let file = Arc::new(open_regular(&root, &entry)?);
                validate_owned(
                    &ownership,
                    entry.to_str().ok_or(DiffRefusal::Unreadable)?,
                    &file,
                    FileType::RegularFile,
                )?;
                entries.push(OwnedName {
                    parent: root.clone(),
                    name: entry,
                    file,
                    kind: FileType::RegularFile,
                });
            }
            Some("refs") => {
                let refs = Arc::new(File::from(
                    open_dir(root.as_ref(), "refs").map_err(|_| DiffRefusal::Unreadable)?,
                ));
                private_dir(refs.as_ref())?;
                validate_owned(&ownership, "refs", &refs, FileType::Directory)?;
                if !names(&refs, budget)?.is_empty() {
                    return Err(DiffRefusal::Unreadable);
                }
                entries.push(OwnedName {
                    parent: root.clone(),
                    name: entry,
                    file: refs,
                    kind: FileType::Directory,
                });
            }
            Some("objects") => recover_objects(&root, &ownership, &mut entries, budget)?,
            _ => return Err(DiffRefusal::Unreadable),
        }
    }
    if Stamp::file(&root)? != root_stamp || Stamp::file(&marker)? != marker_stamp {
        return Err(DiffRefusal::Unreadable);
    }
    let layout = OwnedLayout {
        parent,
        name: name.to_owned(),
        root,
        entries,
        copied_stamps: vec![(marker, marker_stamp)],
        budget: budget.clone(),
    };
    layout.verify()?;
    Ok(layout)
}

fn recover_objects(
    root: &Arc<File>,
    ownership: &Ownership,
    entries: &mut Vec<OwnedName>,
    budget: &Budget,
) -> Result<(), DiffRefusal> {
    let objects = Arc::new(File::from(
        open_dir(root.as_ref(), "objects").map_err(|_| DiffRefusal::Unreadable)?,
    ));
    private_dir(objects.as_ref())?;
    validate_owned(ownership, "objects", &objects, FileType::Directory)?;
    entries.push(OwnedName {
        parent: root.clone(),
        name: "objects".into(),
        file: objects.clone(),
        kind: FileType::Directory,
    });
    for directory in names(&objects, budget)? {
        let Some(text) = directory.to_str() else {
            return Err(DiffRefusal::Unreadable);
        };
        if text != "pack" && !hex(directory.as_bytes(), 2) {
            return Err(DiffRefusal::Unreadable);
        }
        let pack = text == "pack";
        let directory_path = format!("objects/{text}");
        let child = Arc::new(File::from(
            open_dir(objects.as_ref(), &directory).map_err(|_| DiffRefusal::Unreadable)?,
        ));
        private_dir(child.as_ref())?;
        validate_owned(ownership, &directory_path, &child, FileType::Directory)?;
        entries.push(OwnedName {
            parent: objects.clone(),
            name: directory,
            file: child.clone(),
            kind: FileType::Directory,
        });
        for leaf in names(&child, budget)? {
            if !(if pack {
                leaf.to_str().and_then(pack_leaf).is_some()
            } else {
                hex(leaf.as_bytes(), 38)
            }) {
                return Err(DiffRefusal::Unreadable);
            }
            let file = Arc::new(open_regular(&child, &leaf)?);
            validate_owned(
                ownership,
                &format!(
                    "{directory_path}/{}",
                    leaf.to_str().ok_or(DiffRefusal::Unreadable)?
                ),
                &file,
                FileType::RegularFile,
            )?;
            entries.push(OwnedName {
                parent: child.clone(),
                name: leaf,
                file,
                kind: FileType::RegularFile,
            });
        }
    }

    Ok(())
}

type Ownership = BTreeMap<String, (FileType, RootIdentity)>;

fn read_ownership(
    marker: &File,
    expected: &[u8],
    budget: &Budget,
) -> Result<Ownership, DiffRefusal> {
    let mut input = BufReader::new(marker);
    let mut header = vec![0; expected.len()];
    input
        .read_exact(&mut header)
        .map_err(|_| DiffRefusal::Unreadable)?;
    if header != expected {
        return Err(DiffRefusal::Unreadable);
    }
    let mut ownership = BTreeMap::new();
    loop {
        budget.check()?;
        let mut bytes = Vec::new();
        (&mut input)
            .take(HEAD_MAX + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| DiffRefusal::Unreadable)?;
        if bytes.is_empty() {
            break;
        }
        if bytes.len() as u64 > HEAD_MAX || bytes.last() != Some(&b'\n') {
            return Err(DiffRefusal::Unreadable);
        }
        let text =
            std::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| DiffRefusal::Unreadable)?;
        let mut fields = text.split(' ');
        let kind = match fields.next() {
            Some("d") => FileType::Directory,
            Some("f") => FileType::RegularFile,
            _ => return Err(DiffRefusal::Unreadable),
        };
        let device = fields
            .next()
            .and_then(|word| word.parse().ok())
            .ok_or(DiffRefusal::Unreadable)?;
        let inode = fields
            .next()
            .and_then(|word| word.parse().ok())
            .ok_or(DiffRefusal::Unreadable)?;
        let path = fields.next().ok_or(DiffRefusal::Unreadable)?;
        if fields.next().is_some()
            || !owned_path(path, kind)
            || ownership
                .insert(path.to_owned(), (kind, RootIdentity { device, inode }))
                .is_some()
        {
            return Err(DiffRefusal::Unreadable);
        }
    }
    Ok(ownership)
}

fn owned_path(path: &str, kind: FileType) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    match (parts.as_slice(), kind) {
        (["config" | "HEAD" | "index" | "shallow"], FileType::RegularFile)
        | (["objects" | "refs"], FileType::Directory) => true,
        (["objects", directory], FileType::Directory) => {
            *directory == "pack" || hex(directory.as_bytes(), 2)
        }
        (["objects", "pack", leaf], FileType::RegularFile) => pack_leaf(leaf).is_some(),
        (["objects", directory, leaf], FileType::RegularFile) => {
            hex(directory.as_bytes(), 2) && hex(leaf.as_bytes(), 38)
        }
        _ => false,
    }
}

fn validate_owned(
    ownership: &Ownership,
    path: &str,
    file: &File,
    kind: FileType,
) -> Result<(), DiffRefusal> {
    if ownership.get(path) != Some(&(kind, identity(file)?)) {
        return Err(DiffRefusal::Unreadable);
    }
    Ok(())
}

fn private_dir(fd: impl std::os::fd::AsFd) -> Result<(), DiffRefusal> {
    let stat = fstat(fd).map_err(|_| DiffRefusal::Unreadable)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_mode & 0o077 != 0
        || stat.st_uid != nix::unistd::geteuid().as_raw()
    {
        return Err(DiffRefusal::Unreadable);
    }
    Ok(())
}

fn single_component(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes != b"."
        && bytes != b".."
        && !bytes.iter().any(|byte| matches!(byte, 0 | b'/'))
}

fn marker_bytes(root: RootIdentity) -> Vec<u8> {
    format!("dudes-content-shadow-v1\n{}:{}\n", root.device, root.inode).into_bytes()
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

fn hex(bytes: &[u8], length: usize) -> bool {
    bytes.len() == length
        && bytes.iter().all(u8::is_ascii_hexdigit)
        && !bytes.iter().any(u8::is_ascii_uppercase)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    SourceOpened,
    DestinationLinked,
    Staged,
}

struct PendingObject {
    parent: Arc<File>,
    name: OsString,
    source: Arc<File>,
}

/// Hardlinks share mutable bytes. Verification detects observed races, not every ABA write.
pub(super) struct ContentShadow {
    dir: PathBuf,
    root: Root,
    origin: Option<Root>,
    head: GitCommit,
    index_digest: String,
    index_summary: parse::IndexSummary,
    layout: OwnedLayout,
    bindings: Vec<Binding>,
    destination_stamps: Vec<Stamp>,
    root_stamp: Stamp,
}

impl ContentShadow {
    pub(super) fn build(
        scratch: &Path,
        name: &str,
        tree: &Tree,
        expected: Option<RootIdentity>,
        budget: &Budget,
    ) -> Result<Self, DiffRefusal> {
        Self::build_with(scratch, name, tree, expected, budget, |_, _| {})
    }

    fn build_with(
        scratch: &Path,
        name: &str,
        tree: &Tree,
        expected: Option<RootIdentity>,
        budget: &Budget,
        mut barrier: impl FnMut(Stage, &str),
    ) -> Result<Self, DiffRefusal> {
        budget.check()?;
        let path = match tree {
            Tree::Checkout(path) => path,
            Tree::Leased(lease) => &lease.path,
        };
        let root = Root::open(path, expected, budget)?;
        let root_fd = Arc::new(File::from(
            root.fd().try_clone().map_err(|_| DiffRefusal::Unreadable)?,
        ));
        let mut bindings = Vec::new();
        let (own, common, origin) = match tree {
            Tree::Checkout(_) => {
                let git = bound_dir(&root_fd, OsStr::new(".git"), &mut bindings)?;
                (git.clone(), git, None)
            }
            Tree::Leased(lease) => {
                let origin = Root::open(&lease.origin, None, budget)
                    .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                let origin_fd = Arc::new(File::from(
                    origin
                        .fd()
                        .try_clone()
                        .map_err(|_| DiffRefusal::Unreadable)?,
                ));
                let common = bound_dir(&origin_fd, OsStr::new(".git"), &mut bindings)
                    .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                let worktrees = bound_dir(&common, OsStr::new("worktrees"), &mut bindings)
                    .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                let own = bound_dir(
                    &worktrees,
                    OsStr::new(&lease.worktree.to_string()),
                    &mut bindings,
                )
                .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                verify_link_target_identity(
                    &root_fd,
                    ".git",
                    b"gitdir: ",
                    &own,
                    &mut bindings,
                    budget,
                )
                .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                verify_link_target_identity(&own, "commondir", b"", &common, &mut bindings, budget)
                    .map_err(|_| DiffRefusal::WorktreeMismatch)?;
                (own, common, Some(origin))
            }
        };
        absent(&own, "reftable")?;
        absent(&common, "reftable")?;
        if let Some(refs) = optional_dir(&common, OsStr::new("refs"), &mut bindings)? {
            absent(&refs, "replace")?;
        }
        let head = resolve_head(&own, &common, &mut bindings, budget)?;
        let mut layout = OwnedLayout::create(scratch, name, budget)?;
        layout.put(&layout.root.clone(), "config", CONFIG.as_bytes())?;
        layout.put(&layout.root.clone(), "HEAD", format!("{head}\n").as_bytes())?;
        layout.directory(&layout.root.clone(), "refs")?;
        let index = read_small(&own, "index", INDEX_MAX, &mut bindings, budget)?;
        let index_digest = encode_hex(&Sha256::digest(index.as_deref().unwrap_or_default()));
        let index_summary = index
            .as_deref()
            .map(parse::index_summary)
            .transpose()
            .map_err(|_| DiffRefusal::UnsupportedStorage)?
            .unwrap_or_default();
        if let Some(index) = index {
            layout.put(&layout.root.clone(), "index", &index)?;
        }
        if let Some(shallow) = read_small(&common, "shallow", SHALLOW_MAX, &mut bindings, budget)? {
            if !parse::is_shallow(&shallow) {
                return Err(DiffRefusal::UnsupportedStorage);
            }
            layout.put(&layout.root.clone(), "shallow", &shallow)?;
        }
        stage_objects(&common, &mut bindings, &mut layout, budget, &mut barrier)?;
        barrier(Stage::Staged, "");
        let destination_stamps = layout
            .entries
            .iter()
            .map(|entry| Stamp::file(&entry.file))
            .collect::<Result<Vec<_>, _>>()?;
        let root_stamp = Stamp::file(&layout.root)?;
        let shadow = Self {
            root_stamp,
            index_digest,
            index_summary,
            dir: scratch.join(name),
            root,
            origin,
            head,
            layout,
            bindings,
            destination_stamps,
        };
        shadow.verify(budget)?;
        Ok(shadow)
    }

    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }
    pub(super) fn root(&self) -> &Root {
        &self.root
    }
    pub(super) fn head(&self) -> GitCommit {
        self.head
    }
    pub(super) fn index_digest(&self) -> &str {
        &self.index_digest
    }

    pub(super) fn tracked_names(&self) -> &std::collections::BTreeSet<Vec<u8>> {
        &self.index_summary.names
    }
    pub(super) fn has_gitlinks(&self) -> bool {
        self.index_summary.has_gitlinks
    }

    pub(super) fn has_hidden_entries(&self) -> bool {
        self.index_summary.has_hidden_entries
    }

    pub(super) fn reclaim(scratch: &Path, budget: &Budget) -> Result<usize, DiffRefusal> {
        budget.check()?;
        if !scratch.exists() {
            return Ok(0);
        }
        let parent = Root::open(scratch, None, budget)?;
        private_dir(parent.fd())?;
        let parent_file = File::from(
            parent
                .fd()
                .try_clone()
                .map_err(|_| DiffRefusal::Unreadable)?,
        );
        let mut reclaimed = 0;
        for name in names(&parent_file, budget)? {
            budget.check()?;
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(layout) = recover_layout(scratch, parent.identity(), name, budget) else {
                budget.check()?;
                continue;
            };
            layout.cleanup();
            if matches!(
                statat(&parent_file, name, AtFlags::SYMLINK_NOFOLLOW),
                Err(Errno::NOENT)
            ) {
                reclaimed += 1;
            }
        }
        budget.check()?;
        Ok(reclaimed)
    }

    pub(super) fn verify(&self, budget: &Budget) -> Result<(), DiffRefusal> {
        budget.check()?;
        self.root.verify()?;
        if let Some(origin) = &self.origin {
            origin.verify()?;
        }
        self.layout.verify()?;
        if Stamp::file(&self.layout.root)? != self.root_stamp {
            return Err(DiffRefusal::Unreadable);
        }
        for binding in &self.bindings {
            budget.check()?;
            binding.verify()?;
        }
        for (entry, stamp) in self.layout.entries.iter().zip(&self.destination_stamps) {
            budget.check()?;
            if Stamp::file(&entry.file)? != *stamp {
                return Err(DiffRefusal::Unreadable);
            }
        }
        self.root.verify()?;
        budget.check()
    }
}

fn stage_objects(
    common: &Arc<File>,
    bindings: &mut Vec<Binding>,
    layout: &mut OwnedLayout,
    budget: &Budget,
    barrier: &mut impl FnMut(Stage, &str),
) -> Result<(), DiffRefusal> {
    let objects = bound_dir(common, OsStr::new("objects"), bindings)?;
    let destination = layout.directory(&layout.root.clone(), "objects")?;
    let mut pending = Vec::new();
    let mut packs: BTreeMap<String, [Option<Arc<File>>; 2]> = BTreeMap::new();
    for name in names(&objects, budget)? {
        let bytes = name.as_bytes();
        if bytes == b"info" {
            let info = bound_dir(&objects, &name, bindings)?;
            absent(&info, "alternates")?;
            absent(&info, "http-alternates")?;
            continue;
        }
        if bytes == b"maintenance.lock" {
            return Err(DiffRefusal::Busy);
        }
        if bytes != b"pack" && !hex(bytes, 2) {
            return Err(DiffRefusal::UnsupportedStorage);
        }
        let from = bound_dir(&objects, &name, bindings)?;
        let directory = std::str::from_utf8(bytes).map_err(|_| DiffRefusal::UnsupportedStorage)?;
        let to = layout.directory(&destination, directory)?;
        for leaf in names(&from, budget)? {
            let leaf_name = leaf.to_str().ok_or(DiffRefusal::UnsupportedStorage)?;
            let pack = if directory == "pack" {
                if ignored_pack_auxiliary(leaf_name) {
                    let stat = statat(from.as_ref(), &leaf, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(|_| DiffRefusal::Unreadable)?;
                    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                        return Err(DiffRefusal::UnsupportedStorage);
                    }
                    continue;
                }
                let (id, extension) =
                    pack_leaf(leaf_name).ok_or(DiffRefusal::UnsupportedStorage)?;
                Some((id.to_owned(), extension))
            } else {
                if !hex(leaf.as_bytes(), 38) {
                    return Err(DiffRefusal::UnsupportedStorage);
                }
                None
            };
            budget.check()?;
            let source = Arc::new(open_regular(&from, &leaf)?);
            let original = identity(source.as_ref())?;
            barrier(Stage::SourceOpened, leaf_name);
            budget.check()?;
            layout.record(&to, &leaf, original, FileType::RegularFile)?;
            linkat(from.as_ref(), &leaf, to.as_ref(), &leaf, AtFlags::empty())
                .map_err(|_| DiffRefusal::UnsupportedStorage)?;
            barrier(Stage::DestinationLinked, leaf_name);
            let linked = match open_regular(&to, &leaf) {
                Ok(file) => Arc::new(file),
                Err(error) => {
                    layout.disarm_recovery();
                    return Err(error);
                }
            };
            if identity(linked.as_ref())? != original || identity(source.as_ref())? != original {
                layout.disarm_recovery();
                return Err(DiffRefusal::Unreadable);
            }
            layout.entries.push(OwnedName {
                parent: to.clone(),
                name: leaf.clone(),
                file: linked.clone(),
                kind: FileType::RegularFile,
            });
            verify_name(&from, &leaf, &source, FileType::RegularFile)?;
            verify_name(&to, &leaf, &linked, FileType::RegularFile)?;
            if let Some((id, extension)) = pack {
                packs.entry(id).or_default()[extension] = Some(source.clone());
            }
            pending.push(PendingObject {
                parent: from.clone(),
                name: leaf,
                source,
            });
        }
    }
    // Link creation changes ctime and nlink. Sample object stamps only after all links exist.
    for object in pending {
        budget.check()?;
        bindings.push(Binding::new(object.parent, object.name, object.source)?);
    }
    for (id, pair) in packs {
        budget.check()?;
        check_pack_pair(&id, pair, budget)?;
    }
    Ok(())
}

fn names(parent: &File, budget: &Budget) -> Result<Vec<OsString>, DiffRefusal> {
    let mut listing = Dir::read_from(parent).map_err(|_| DiffRefusal::Unreadable)?;
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        budget.check()?;
        let entry = entry.map_err(|_| DiffRefusal::Unreadable)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(bytes).to_owned());
    }
    names.sort();
    Ok(names)
}

fn ignored_pack_auxiliary(name: &str) -> bool {
    if name == "multi-pack-index" {
        return true;
    }
    let Some((id, extension)) = name
        .strip_prefix("pack-")
        .and_then(|name| name.split_once('.'))
    else {
        return false;
    };
    hex(id.as_bytes(), 40) && matches!(extension, "rev" | "bitmap" | "keep")
}

fn pack_leaf(name: &str) -> Option<(&str, usize)> {
    let name = name.strip_prefix("pack-")?;
    let (id, extension) = name.split_once('.')?;
    if !hex(id.as_bytes(), 40) {
        return None;
    }
    Some((
        id,
        match extension {
            "pack" => 0,
            "idx" => 1,
            _ => return None,
        },
    ))
}

fn read_at(file: &File, at: u64, bytes: &mut [u8], budget: &Budget) -> Result<(), DiffRefusal> {
    budget.check()?;
    let mut file = file;
    file.seek(SeekFrom::Start(at))
        .and_then(|_| file.read_exact(bytes))
        .map_err(|_| DiffRefusal::UnsupportedStorage)
}

fn check_pack_pair(
    id: &str,
    pair: [Option<Arc<File>>; 2],
    budget: &Budget,
) -> Result<(), DiffRefusal> {
    let [Some(pack), Some(index)] = pair else {
        return Err(DiffRefusal::UnsupportedStorage);
    };
    let pack_length = pack.metadata().map_err(|_| DiffRefusal::Unreadable)?.len();
    if pack_length < 32 {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let mut header = [0; 12];
    read_at(&pack, 0, &mut header, budget)?;
    let version = u32::from_be_bytes(
        header[4..8]
            .try_into()
            .map_err(|_| DiffRefusal::UnsupportedStorage)?,
    );
    if &header[..4] != b"PACK" || !matches!(version, 2 | 3) {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let mut checksum = [0; 20];
    read_at(&pack, pack_length - 20, &mut checksum, budget)?;
    let encoded: String = encode_hex(&checksum);
    if encoded != id {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let index_length = index.metadata().map_err(|_| DiffRefusal::Unreadable)?.len();
    if index_length < 1064 {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let mut prefix = [0; 8];
    read_at(&index, 0, &mut prefix, budget)?;
    let version_two = prefix[..4] == [255, b't', b'O', b'c'];
    let fanout_at = if version_two {
        if prefix[4..] != [0, 0, 0, 2] {
            return Err(DiffRefusal::UnsupportedStorage);
        }
        8
    } else {
        0
    };
    let mut fanout = [0; 1024];
    read_at(&index, fanout_at, &mut fanout, budget)?;
    let mut previous = 0_u32;
    for word in fanout.as_chunks::<4>().0 {
        let count = u32::from_be_bytes(*word);
        if count < previous {
            return Err(DiffRefusal::UnsupportedStorage);
        }
        previous = count;
    }
    if previous.to_be_bytes() != header[8..12] {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let count = u64::from(previous);
    let minimum = fanout_at + 1024 + count * if version_two { 28 } else { 24 } + 40;
    if index_length < minimum
        || (!version_two && index_length != minimum)
        || (index_length - minimum) % 8 != 0
    {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    let mut named_checksum = [0; 20];
    read_at(&index, index_length - 40, &mut named_checksum, budget)?;
    if named_checksum != checksum {
        return Err(DiffRefusal::UnsupportedStorage);
    }
    Ok(())
}

#[cfg(test)]
mod process_tests;

#[cfg(test)]
mod tests {
    use super::super::files;
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, atomic::AtomicBool};
    pub(super) fn budget() -> files::Budget {
        files::Budget::new(Arc::new(AtomicBool::new(false)))
    }
    pub(super) struct Fixture {
        pub(super) temp: tempfile::TempDir,
        pub(super) tree: PathBuf,
        pub(super) scratch: PathBuf,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let tree = temp.path().join("tree");
            let scratch = temp.path().join("scratch");
            fs::create_dir_all(tree.join(".git/objects/ab")).unwrap();
            fs::create_dir_all(tree.join(".git/refs/heads")).unwrap();
            fs::write(tree.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
            fs::write(
                tree.join(".git/refs/heads/main"),
                format!("{}\n", "a".repeat(40)),
            )
            .unwrap();
            fs::write(
                tree.join(".git/objects/ab").join("c".repeat(38)),
                b"object bytes",
            )
            .unwrap();
            Self {
                temp,
                tree,
                scratch,
            }
        }
        pub(super) fn build(&self) -> ContentShadow {
            ContentShadow::build(
                &self.scratch,
                "sample",
                &Tree::Checkout(self.tree.clone()),
                None,
                &budget(),
            )
            .unwrap()
        }
        pub(super) fn leaf(&self) -> PathBuf {
            self.tree.join(".git/objects/ab").join("c".repeat(38))
        }
    }
    #[test]
    fn content_shadow_stages_verified_hardlinks_and_detaches_resolved_head() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.leaf()).unwrap();
        let shadow = fixture.build();
        assert_eq!(
            fs::read(shadow.dir().join("HEAD")).unwrap(),
            format!("{}\n", "a".repeat(40)).as_bytes()
        );
        assert_eq!(shadow.head(), GitCommit::parse(&"a".repeat(40)).unwrap());
        let dest =
            fs::symlink_metadata(shadow.dir().join("objects/ab").join("c".repeat(38))).unwrap();
        assert!(dest.is_file());
        assert_eq!((dest.dev(), dest.ino()), (original.dev(), original.ino()));
        assert_eq!(dest.nlink(), original.nlink() + 1);
        assert_eq!(shadow.verify(&budget()), Ok(()));
        let dir = shadow.dir().to_path_buf();
        drop(shadow);
        assert!(!dir.exists());
        assert_eq!(
            fs::metadata(fixture.leaf()).unwrap().nlink(),
            original.nlink()
        );
    }
    #[test]
    fn content_shadow_detects_in_place_bytes_and_named_replacement() {
        for replacement in [false, true] {
            let fixture = Fixture::new();
            let shadow = fixture.build();
            if replacement {
                fs::rename(fixture.leaf(), fixture.temp.path().join("old-object")).unwrap();
            }
            fs::write(fixture.leaf(), b"changed bytes").unwrap();
            assert_eq!(shadow.verify(&budget()), Err(DiffRefusal::Unreadable));
        }
    }
    #[test]
    fn content_shadow_refuses_replaced_root_and_linked_checkout() {
        let fixture = Fixture::new();
        let shadow = fixture.build();
        let id = shadow.root().identity();
        let old = fixture.temp.path().join("old-tree");
        fs::rename(&fixture.tree, &old).unwrap();
        fs::create_dir(&fixture.tree).unwrap();
        assert_eq!(shadow.verify(&budget()), Err(DiffRefusal::RootMismatch));
        fs::remove_dir(&fixture.tree).unwrap();
        symlink(&old, &fixture.tree).unwrap();
        assert!(matches!(
            ContentShadow::build(
                &fixture.scratch,
                "second",
                &Tree::Checkout(fixture.tree.clone()),
                Some(id),
                &budget()
            ),
            Err(DiffRefusal::RootMismatch)
        ));
    }
    #[test]
    fn content_shadow_cancellation_happens_before_filesystem_work() {
        let cancelled = files::Budget::new(Arc::new(AtomicBool::new(true)));
        assert!(matches!(
            ContentShadow::build(
                Path::new("/nonexistent"),
                "sample",
                &Tree::Checkout(PathBuf::from("/nonexistent")),
                None,
                &cancelled
            ),
            Err(DiffRefusal::TimedOut)
        ));
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::tests::Fixture;
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn content_shadow_reclaims_cancelled_owned_layout_and_preserves_unknown_entries() {
        let fixture = Fixture::new();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let budget = Budget::new(cancelled.clone());
        let shadow = ContentShadow::build(
            &fixture.scratch,
            "sample",
            &Tree::Checkout(fixture.tree.clone()),
            None,
            &budget,
        )
        .unwrap();
        cancelled.store(true, Ordering::Release);
        drop(shadow);
        assert!(fixture.scratch.join("sample").exists());
        assert_eq!(
            ContentShadow::reclaim(&fixture.scratch, &super::tests::budget()),
            Ok(1)
        );
        assert!(!fixture.scratch.join("sample").exists());
        let mut shadow = fixture.build();
        std::fs::write(shadow.dir().join("unknown"), "keep").unwrap();
        shadow.layout.budget = Budget::new(Arc::new(std::sync::atomic::AtomicBool::new(true)));
        drop(shadow);
        assert_eq!(
            ContentShadow::reclaim(&fixture.scratch, &super::tests::budget()),
            Ok(0)
        );
        assert_eq!(
            std::fs::read(fixture.scratch.join("sample/unknown")).unwrap(),
            b"keep"
        );
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::tests::{Fixture, budget};
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn content_shadow_rejects_source_and_destination_swaps_before_the_git_gate() {
        for source in [true, false] {
            for special in ["regular", "symlink", "fifo", "directory", "socket"] {
                let fixture = Fixture::new();
                drop(fixture.build());
                let outside = fixture.temp.path().join("outside");
                fs::write(&outside, b"outside marker").unwrap();
                let mut fired = false;
                let result = ContentShadow::build_with(
                    &fixture.scratch,
                    "sample",
                    &Tree::Checkout(fixture.tree.clone()),
                    None,
                    &budget(),
                    |stage, name| {
                        if fired
                            || stage
                                != if source {
                                    Stage::SourceOpened
                                } else {
                                    Stage::DestinationLinked
                                }
                        {
                            return;
                        }
                        fired = true;
                        let target = if source {
                            fixture.leaf()
                        } else {
                            fixture.scratch.join("sample/objects/ab").join(name)
                        };
                        fs::remove_file(&target).unwrap();
                        match special {
                            "regular" => fs::write(&target, b"substituted").unwrap(),
                            "symlink" => symlink(&outside, &target).unwrap(),
                            "fifo" => {
                                nix::unistd::mkfifo(&target, nix::sys::stat::Mode::S_IRUSR)
                                    .unwrap();
                            }
                            "directory" => fs::create_dir(&target).unwrap(),
                            "socket" => {
                                let socket = fixture.temp.path().join("socket");
                                drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
                                fs::rename(socket, &target).unwrap();
                            }
                            _ => unreachable!(),
                        }
                    },
                );
                assert!(fired);
                assert!(result.is_err(), "source={source}, kind={special}");
                assert_eq!(fs::read(&outside).unwrap(), b"outside marker");
            }
        }
    }

    #[test]
    fn content_shadow_refuses_destination_collisions_and_new_control_files() {
        for control in ["objects/ab/collision", "commondir", "config"] {
            let fixture = Fixture::new();
            drop(fixture.build());
            let mut fired = false;
            let result = ContentShadow::build_with(
                &fixture.scratch,
                "sample",
                &Tree::Checkout(fixture.tree.clone()),
                None,
                &budget(),
                |stage, name| {
                    if fired || stage != Stage::SourceOpened {
                        return;
                    }
                    fired = true;
                    let path = if control.ends_with("collision") {
                        fixture.scratch.join("sample/objects/ab").join(name)
                    } else {
                        fixture.scratch.join("sample").join(control)
                    };
                    fs::write(path, b"outside marker").unwrap();
                },
            );
            assert!(fired);
            assert!(result.is_err(), "{control}");
        }
    }

    #[test]
    fn content_shadow_unsupported_storage_and_special_administration_refuse() {
        for control in [
            ".git/objects/info/alternates",
            ".git/objects/info/http-alternates",
            ".git/objects/pack/pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.promisor",
            ".git/objects/pack/pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.rev",
            ".git/objects/xx",
            ".git/refs/replace",
            ".git/reftable",
            ".git/index",
            ".git/shallow",
            ".git/HEAD",
        ] {
            let fixture = Fixture::new();
            drop(fixture.build());
            let path = fixture.tree.join(control);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            symlink(fixture.temp.path().join("outside"), &path).unwrap();
            assert!(
                ContentShadow::build(
                    &fixture.scratch,
                    "sample",
                    &Tree::Checkout(fixture.tree.clone()),
                    None,
                    &budget()
                )
                .is_err(),
                "{control}"
            );
        }
    }

    #[test]
    fn content_shadow_staging_cancellation_retains_only_reclaimable_owned_names() {
        let fixture = Fixture::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let budget = Budget::new(cancellation.clone());
        let result = ContentShadow::build_with(
            &fixture.scratch,
            "sample",
            &Tree::Checkout(fixture.tree.clone()),
            None,
            &budget,
            |stage, _| {
                if stage == Stage::DestinationLinked {
                    cancellation.store(true, Ordering::Release);
                }
            },
        );
        assert!(matches!(result, Err(DiffRefusal::TimedOut)));
        assert_eq!(
            ContentShadow::reclaim(&fixture.scratch, &super::tests::budget()),
            Ok(1)
        );
    }

    fn pack_pair(fixture: &Fixture, mixed: bool) {
        let directory = fixture.tree.join(".git/objects/pack");
        fs::create_dir(&directory).unwrap();
        let name = format!("pack-{}", "a".repeat(40));
        let mut pack = b"PACK\0\0\0\x02\0\0\0\0".to_vec();
        pack.extend([0xaa; 20]);
        fs::write(directory.join(format!("{name}.pack")), pack).unwrap();
        let mut index = b"\xfftOc\0\0\0\x02".to_vec();
        index.extend([0; 1024]);
        index.extend([if mixed { 0xbb } else { 0xaa }; 20]);
        index.extend([0xcc; 20]);
        fs::write(directory.join(format!("{name}.idx")), index).unwrap();
    }

    #[test]
    fn content_shadow_pack_index_generations_are_paired_and_stamped() {
        let fixture = Fixture::new();
        pack_pair(&fixture, false);
        let shadow = fixture.build();
        assert_eq!(shadow.verify(&budget()), Ok(()));
        let index = fixture
            .tree
            .join(format!(".git/objects/pack/pack-{}.idx", "a".repeat(40)));
        let mut bytes = fs::read(&index).unwrap();
        bytes[1050] ^= 1;
        fs::write(&index, bytes).unwrap();
        assert_eq!(shadow.verify(&budget()), Err(DiffRefusal::Unreadable));
        drop(shadow);
        let fixture = Fixture::new();
        pack_pair(&fixture, true);
        assert!(matches!(
            ContentShadow::build(
                &fixture.scratch,
                "sample",
                &Tree::Checkout(fixture.tree.clone()),
                None,
                &budget()
            ),
            Err(DiffRefusal::UnsupportedStorage)
        ));
    }

    #[test]
    fn content_shadow_cleanup_never_walks_a_replacement_root_or_directory() {
        for nested in [false, true] {
            let fixture = Fixture::new();
            let shadow = fixture.build();
            let target = if nested {
                shadow.dir().join("objects/ab")
            } else {
                shadow.dir().to_path_buf()
            };
            fs::rename(&target, fixture.temp.path().join("detached")).unwrap();
            fs::create_dir(&target).unwrap();
            fs::write(target.join("outside"), b"keep").unwrap();
            drop(shadow);
            assert_eq!(fs::read(target.join("outside")).unwrap(), b"keep");
        }
    }

    #[test]
    fn content_shadow_resolves_packed_head_and_binds_absent_index() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.tree.join(".git/refs/heads/main")).unwrap();
        fs::write(
            fixture.tree.join(".git/packed-refs"),
            format!(
                "# pack-refs with: peeled\n{} refs/heads/main\n",
                "b".repeat(40)
            ),
        )
        .unwrap();
        let shadow = fixture.build();
        assert_eq!(shadow.head(), GitCommit::parse(&"b".repeat(40)).unwrap());
        assert_eq!(
            shadow.index_digest(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        fs::write(fixture.tree.join(".git/index"), b"new").unwrap();
        assert_eq!(shadow.verify(&budget()), Err(DiffRefusal::Unreadable));
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::tests::{Fixture, budget};
    use super::*;
    #[test]
    fn content_shadow_preserves_unverified_destination_replacements() {
        for symlink in [false, true] {
            let fixture = Fixture::new();
            let target = fixture
                .scratch
                .join("sample/objects/ab")
                .join("c".repeat(38));
            let result = ContentShadow::build_with(
                &fixture.scratch,
                "sample",
                &Tree::Checkout(fixture.tree.clone()),
                None,
                &budget(),
                |stage, _| {
                    if stage != Stage::DestinationLinked {
                        return;
                    }
                    std::fs::remove_file(&target).unwrap();
                    if symlink {
                        std::os::unix::fs::symlink("outside", &target).unwrap();
                    } else {
                        std::fs::write(&target, b"foreign replacement").unwrap();
                    }
                },
            );
            assert!(result.is_err());
            assert!(
                std::fs::symlink_metadata(&target).is_ok(),
                "symlink={symlink}"
            );
            assert_eq!(ContentShadow::reclaim(&fixture.scratch, &budget()), Ok(0));
            assert!(std::fs::symlink_metadata(&target).is_ok());
        }
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::tests::{Fixture, budget};
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;
    #[test]
    fn content_shadow_expired_budget_and_original_descriptors_are_finite() {
        let fixture = Fixture::new();
        let expired = Budget::until(Instant::now(), Arc::new(AtomicBool::new(false)));
        assert!(matches!(
            ContentShadow::build(
                &fixture.scratch,
                "sample",
                &Tree::Checkout(fixture.tree.clone()),
                None,
                &expired
            ),
            Err(DiffRefusal::TimedOut)
        ));
        assert!(!fixture.scratch.exists());
        for _ in 0..20 {
            let shadow = fixture.build();
            let held: Vec<_> = shadow
                .bindings
                .iter()
                .map(|binding| Arc::downgrade(&binding.file))
                .chain(
                    shadow
                        .layout
                        .entries
                        .iter()
                        .map(|entry| Arc::downgrade(&entry.file)),
                )
                .collect();
            let root = Arc::downgrade(&shadow.layout.root);
            assert_eq!(shadow.verify(&budget()), Ok(()));
            drop(shadow);
            assert!(root.upgrade().is_none());
            assert!(held.iter().all(|file| file.upgrade().is_none()));
            assert_eq!(std::fs::read_dir(&fixture.scratch).unwrap().count(), 0);
        }
    }
}

#[cfg(test)]
mod restart_identity_tests {
    use super::tests::{Fixture, budget};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[test]
    fn content_shadow_restart_refuses_foreign_known_name_after_abrupt_abandonment() {
        for directory in [false, true] {
            let fixture = Fixture::new();
            let cancelled = Arc::new(AtomicBool::new(false));
            let request = Budget::new(cancelled.clone());
            let shadow = ContentShadow::build(
                &fixture.scratch,
                "sample",
                &Tree::Checkout(fixture.tree.clone()),
                None,
                &request,
            )
            .unwrap();
            let target = if directory {
                shadow.dir().join("objects/ab")
            } else {
                shadow.dir().join("objects/ab").join("c".repeat(38))
            };
            std::fs::rename(&target, fixture.temp.path().join("held")).unwrap();
            if directory {
                std::fs::create_dir(&target).unwrap();
            } else {
                std::fs::write(&target, b"foreign replacement").unwrap();
            }
            cancelled.store(true, Ordering::Release);
            drop(shadow);
            assert_eq!(ContentShadow::reclaim(&fixture.scratch, &budget()), Ok(0));
            assert!(target.exists());
        }
    }
}

#[cfg(test)]
mod auxiliary_tests {
    use super::tests::{Fixture, budget};
    #[test]
    fn content_shadow_ignores_unstaged_auxiliary_metadata_without_reading_its_bytes() {
        let fixture = Fixture::new();
        let info = fixture.tree.join(".git/objects/info");
        std::fs::create_dir(&info).unwrap();
        std::fs::write(info.join("packs"), b"auxiliary bytes").unwrap();
        std::fs::write(info.join("commit-graph"), b"auxiliary bytes").unwrap();
        let pack = fixture.tree.join(".git/objects/pack");
        std::fs::create_dir(&pack).unwrap();
        for extension in ["rev", "bitmap", "keep"] {
            std::fs::write(
                pack.join(format!("pack-{}.{extension}", "a".repeat(40))),
                b"auxiliary bytes",
            )
            .unwrap();
        }
        std::fs::write(pack.join("multi-pack-index"), b"auxiliary bytes").unwrap();
        let shadow = fixture.build();
        assert!(!shadow.dir().join("objects/info").exists());
        assert_eq!(
            std::fs::read_dir(shadow.dir().join("objects/pack"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(shadow.verify(&budget()), Ok(()));
    }
}

#[cfg(test)]
mod index_kind_tests {
    use super::tests::Fixture;
    #[test]
    fn content_shadow_reports_gitlinks_from_the_complete_validated_index() {
        for mode in [0o160_000_u32, 0o100_644] {
            let fixture = Fixture::new();
            let mut index = b"DIRC\0\0\0\x02\0\0\0\x01".to_vec();
            let mut entry = vec![0_u8; 72];
            entry[24..28].copy_from_slice(&mode.to_be_bytes());
            entry[62..68].copy_from_slice(b"module");
            index.extend(entry);
            index.extend([0; 20]);
            std::fs::write(fixture.tree.join(".git/index"), index).unwrap();
            let shadow = fixture.build();
            assert_eq!(shadow.has_gitlinks(), mode == 0o160_000);
        }
    }
}

#[cfg(test)]
mod hidden_index_tests {
    use super::tests::Fixture;
    #[test]
    fn content_shadow_reports_assume_valid_and_skip_worktree_without_a_second_walk() {
        for (flags, extended, hidden) in [
            (0x8000_u16, 0_u16, true),
            (0x4000, 0x4000, true),
            (0, 0, false),
        ] {
            let fixture = Fixture::new();
            let mut index = b"DIRC\0\0\0\x03\0\0\0\x01".to_vec();
            let mut entry = vec![0_u8; 72];
            entry[24..28].copy_from_slice(&0o100_644_u32.to_be_bytes());
            entry[60..62].copy_from_slice(&flags.to_be_bytes());
            let path = if flags & 0x4000 != 0 {
                entry[62..64].copy_from_slice(&extended.to_be_bytes());
                64
            } else {
                62
            };
            entry[path..path + 6].copy_from_slice(b"module");
            index.extend(entry);
            index.extend([0; 20]);
            std::fs::write(fixture.tree.join(".git/index"), index).unwrap();
            let shadow = fixture.build();
            assert_eq!(shadow.has_hidden_entries(), hidden);
        }
    }
}
