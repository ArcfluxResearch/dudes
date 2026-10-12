//! Verify the recorded directory and a lease's exact Git administrative layout.

use dudes_proto::rpc::ticket::TicketCheckoutResult;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckoutError {
    #[error("The recorded checkout directory is no longer available.")]
    Unavailable,
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[error("The recorded lease cannot be verified on this platform.")]
    UnsupportedLease,
}

pub(crate) fn validate(checkout: &TicketCheckoutResult) -> Result<(), CheckoutError> {
    if !checkout.directory.is_absolute() || !checkout.directory.is_dir() {
        return Err(CheckoutError::Unavailable);
    }
    if let Some(lease) = &checkout.worktree {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        unix::validate(&checkout.directory, lease).map_err(|_| CheckoutError::Unavailable)?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = lease;
            return Err(CheckoutError::UnsupportedLease);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Read as _};
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use dudes_proto::rpc::ticket::TicketCheckoutLease;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat};

    const LINK_MAX: u64 = 4 * 1024;

    fn unavailable() -> io::Error {
        io::Error::other("recorded lease mismatch")
    }

    fn directory(parent: impl AsFd, path: impl rustix::path::Arg) -> io::Result<OwnedFd> {
        Ok(openat(
            parent,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    pub(super) fn validate(path: &Path, lease: &TicketCheckoutLease) -> io::Result<()> {
        if !lease.origin.is_absolute() {
            return Err(unavailable());
        }
        let origin = lease.origin.canonicalize()?;
        let root = directory(rustix::fs::CWD, &origin)?;
        let common = directory(&root, ".git")?;
        let worktrees = directory(&common, "worktrees")?;
        let admin = directory(&worktrees, lease.worktree.to_string().as_str())?;
        let tree = directory(rustix::fs::CWD, path)?;
        links_to(&tree, ".git", b"gitdir: ", &admin)?;
        links_to(&admin, "commondir", b"", &common)
    }

    fn links_to(dir: &OwnedFd, name: &str, prefix: &[u8], target: &OwnedFd) -> io::Result<()> {
        let fd = openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let metadata = fstat(&fd)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || metadata.st_size < 0
            || u64::try_from(metadata.st_size).map_err(|_| unavailable())? > LINK_MAX
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        File::from(fd).take(LINK_MAX + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).map_err(|_| unavailable())? > LINK_MAX {
            return Err(unavailable());
        }
        let rest = bytes.strip_prefix(prefix).ok_or_else(unavailable)?;
        let end = rest
            .iter()
            .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
            .map_or(0, |last| last + 1);
        let path = &rest[..end];
        if path.is_empty() || path.iter().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
            return Err(unavailable());
        }
        let there = statat(dir, OsStr::from_bytes(path), AtFlags::empty())?;
        let wanted = fstat(target)?;
        if (there.st_dev, there.st_ino) != (wanted.st_dev, wanted.st_ino) {
            return Err(unavailable());
        }
        Ok(())
    }
}
