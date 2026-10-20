//! What the operator vouches for when they trust a project, and the check that it still holds.
//!
//! A grant names the repository twice, by path and by the device and inode of its directory and
//! of its git common directory, and pins the settings an agent in a copy of it would run under:
//! the `.claude/` tree and `.mcp.json` at the commit a copy starts from, the common directory's
//! `info/attributes`, and its configuration entries less the two prefixes that change in ordinary
//! work. A hire recomputes all of it, so a swapped directory, a committed change to those paths or
//! an edit to the configuration needs the operator again. The check is not a lock: it reads once,
//! and `git worktree add` runs later.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use dudes_proto::record::{FileIdentity, Hex32, TrustPin, TrustedRepository};
use dudes_types::GitCommit;
use sha2::{Digest, Sha256};

use super::git::{Git, GitError};

const QUICK: Duration = Duration::from_secs(20);
/// The most standard output any one read keeps. A repository whose answer is longer cannot be
/// pinned, because a prefix of it would leave the rest unseen.
const READ_CAP: usize = 1024 * 1024;
/// The most of `info/attributes` that is read.
const ATTRIBUTES_CAP: u64 = 1024 * 1024;

/// Why a project cannot be trusted, or no longer matches what was trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustRefusal {
    /// The directory is in no git repository this daemon can tell apart safely.
    NotARepository,
    /// A repository setting the pin cannot cover.
    Unsupported(&'static str),
    /// A path the pin covers is a symbolic link at the commit a copy starts from.
    Symlink,
    /// The repository's answer was too long to read whole.
    TooLarge,
    /// Two pinned entries differ only by letter case, so one file system would show only one.
    CaseVariants,
    /// The repository is not the one that was trusted.
    Moved,
    /// The repository's settings differ from the ones that were trusted.
    Changed,
    /// Git could not answer.
    Git(String),
}

impl std::fmt::Display for TrustRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepository => f.write_str(
                "the folder is not a plain git repository (a linked worktree, a submodule or a \
                 separate git directory cannot be told apart safely)",
            ),
            Self::Unsupported(what) => write!(
                f,
                "the repository's configuration uses {what}, which a trust grant cannot cover"
            ),
            Self::Symlink => {
                f.write_str("`.claude/` or `.mcp.json` holds a symbolic link at the current commit")
            }
            Self::CaseVariants => f.write_str(
                "`.claude/` or `.mcp.json` holds two entries that differ only by letter case",
            ),
            Self::TooLarge => f.write_str("the repository's configuration is too large to pin"),
            Self::Moved => f.write_str("the repository is not the directory that was trusted"),
            Self::Changed => {
                f.write_str("the repository's settings changed since they were trusted")
            }
            Self::Git(text) => write!(f, "git could not answer: {text}"),
        }
    }
}

impl From<GitError> for TrustRefusal {
    fn from(error: GitError) -> Self {
        Self::Git(error.to_string())
    }
}

/// What a grant records and a hire recomputes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vouch {
    /// The repository, by path and identity.
    pub repository: TrustedRepository,
    /// Its pinned settings.
    pub pin: TrustPin,
}

/// Configuration keys no pin can cover: a lower-cased prefix when it ends in a dot, else a name.
const REFUSED_KEYS: [(&str, &str); 5] = [
    ("include.", "`include.*`"),
    ("includeif.", "`includeIf.*`"),
    ("core.attributesfile", "`core.attributesFile`"),
    ("extensions.worktreeconfig", "`extensions.worktreeConfig`"),
    ("filter.", "a `filter.*` driver"),
];

/// Read the repository `directory` is in, as it stands at `HEAD`.
///
/// # Errors
///
/// [`TrustRefusal`] for each reason a grant is refused.
pub fn inspect(git: &Git, directory: &Path) -> Result<Vouch, TrustRefusal> {
    let origin = git
        .toplevel(directory)
        .map_err(|_| TrustRefusal::NotARepository)?;
    if !super::contains(&origin, directory) || super::plain_git_dir(&origin).is_none() {
        return Err(TrustRefusal::NotARepository);
    }
    let base = git.commit(&origin, "HEAD")?;
    vouch_for(git, &origin, base)
}

/// The same read for a hire: recomputed at `base`, and compared with `granted`.
///
/// # Errors
///
/// [`TrustRefusal::Moved`] or [`TrustRefusal::Changed`] when the repository differs from the
/// grant, and the refusals of [`inspect`] when it can no longer be pinned.
pub fn verify(
    git: &Git,
    origin: &Path,
    base: GitCommit,
    granted: &Vouch,
) -> Result<(), TrustRefusal> {
    if origin != granted.repository.directory {
        return Err(TrustRefusal::Moved);
    }
    let now = vouch_for(git, origin, base)?;
    if now.repository != granted.repository {
        return Err(TrustRefusal::Moved);
    }
    if now.pin != granted.pin {
        return Err(TrustRefusal::Changed);
    }
    Ok(())
}

/// Whether `origin` is still the directory a grant vouched for, by path and by the identity of
/// it and of its git common directory, and `project` is still inside it. Reads the file system
/// only, so the loop can ask it at each append.
#[must_use]
pub fn still_there(repository: &TrustedRepository, origin: &Path, project: &Path) -> bool {
    origin == repository.directory
        && super::contains(&repository.directory, project)
        && identity(&repository.directory).is_ok_and(|now| now == repository.directory_identity)
        && identity(&repository.common_dir).is_ok_and(|now| now == repository.common_identity)
}

fn vouch_for(git: &Git, origin: &Path, base: GitCommit) -> Result<Vouch, TrustRefusal> {
    let common = super::plain_git_dir(origin).ok_or(TrustRefusal::NotARepository)?;
    let repository = TrustedRepository {
        directory: origin.to_path_buf(),
        directory_identity: identity(origin)?,
        common_dir: common.clone(),
        common_identity: identity(&common)?,
    };
    let base = base.to_string();
    let (claude_tree, mcp_json) = settings_entries(git, origin, &base)?;
    let pin = TrustPin {
        claude_tree,
        mcp_json,
        attributes: attributes_digest(&common)?,
        config: config_digest(git, origin, &common)?,
    };
    Ok(Vouch { repository, pin })
}

#[cfg(unix)]
fn identity(path: &Path) -> Result<FileIdentity, TrustRefusal> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|_| TrustRefusal::NotARepository)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn identity(_path: &Path) -> Result<FileIdentity, TrustRefusal> {
    Err(TrustRefusal::Unsupported("this platform"))
}

/// What the root tree of `base` holds under the two pinned names.
///
/// Names are matched ignoring ASCII case, because a checkout on a case-insensitive file system
/// puts `.Claude` where the agent reads `.claude`. Every such entry is pinned by name and object
/// id, a symbolic link at any depth is refused, and so are two entries that differ only by case.
fn settings_entries(
    git: &Git,
    origin: &Path,
    base: &str,
) -> Result<(Option<String>, Option<String>), TrustRefusal> {
    let root = listing(git, origin, &["ls-tree", "-z"], base)?;
    let mut claude = None;
    let mut mcp = None;
    for (mode, kind, oid, name) in entries(&root) {
        let slot = match name.to_ascii_lowercase().as_str() {
            ".claude" => &mut claude,
            ".mcp.json" => &mut mcp,
            _ => continue,
        };
        if slot.is_some() {
            return Err(TrustRefusal::CaseVariants);
        }
        if mode == "120000" {
            return Err(TrustRefusal::Symlink);
        }
        if kind == "tree" {
            let inside = listing(git, origin, &["ls-tree", "-r", "-z"], oid)?;
            let mut seen = std::collections::BTreeSet::new();
            for (mode, _, _, path) in entries(&inside) {
                if mode == "120000" {
                    return Err(TrustRefusal::Symlink);
                }
                if !seen.insert(path.to_ascii_lowercase()) {
                    return Err(TrustRefusal::CaseVariants);
                }
            }
        }
        *slot = Some(format!("{name}:{oid}"));
    }
    Ok((claude, mcp))
}

/// The entries of an `ls-tree -z` listing: mode, type, object id and path.
fn entries(listing: &[u8]) -> impl Iterator<Item = (&str, &str, &str, String)> {
    listing.split(|byte| *byte == 0).filter_map(|entry| {
        let at = entry.iter().position(|byte| *byte == b'\t')?;
        let meta = std::str::from_utf8(&entry[..at]).ok()?;
        let mut parts = meta.split(' ');
        Some((
            parts.next()?,
            parts.next()?,
            parts.next()?,
            String::from_utf8_lossy(&entry[at + 1..]).into_owned(),
        ))
    })
}

fn listing(git: &Git, origin: &Path, words: &[&str], rev: &str) -> Result<Vec<u8>, TrustRefusal> {
    let mut command: Vec<&OsStr> = words.iter().map(OsStr::new).collect();
    command.push(OsStr::new(rev));
    let (exit, out, truncated) = git.capture(origin, &command, QUICK, READ_CAP)?;
    if exit != Some(0) {
        return Err(TrustRefusal::Git(format!("ls-tree exited with {exit:?}")));
    }
    if truncated {
        return Err(TrustRefusal::TooLarge);
    }
    Ok(out)
}

fn absent(what: &[u8]) -> Hex32 {
    Hex32::new(Sha256::digest(what).into())
}

fn attributes_digest(common: &Path) -> Result<Hex32, TrustRefusal> {
    use std::io::Read;
    let path = common.join("info").join("attributes");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(absent(b"dudes.trust.attributes.absent"));
        }
        Err(error) => return Err(TrustRefusal::Git(format!("info/attributes: {error}"))),
    };
    let mut bytes = Vec::new();
    file.take(ATTRIBUTES_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| TrustRefusal::Git(format!("info/attributes: {error}")))?;
    if bytes.len() as u64 > ATTRIBUTES_CAP {
        return Err(TrustRefusal::TooLarge);
    }
    let mut hash = Sha256::new();
    hash.update(b"dudes.trust.attributes");
    hash.update(&bytes);
    Ok(Hex32::new(hash.finalize().into()))
}

/// The digest of the common directory's configuration entries less `branch.` and `remote.`, after
/// refusing the keys no pin can cover.
fn config_digest(git: &Git, origin: &Path, common: &Path) -> Result<Hex32, TrustRefusal> {
    let file = common.join("config");
    if !file.exists() {
        return Ok(absent(b"dudes.trust.config.absent"));
    }
    let (exit, out, truncated) = git.capture(
        origin,
        &[
            OsStr::new("config"),
            OsStr::new("--file"),
            file.as_os_str(),
            OsStr::new("--list"),
            OsStr::new("--null"),
        ],
        QUICK,
        READ_CAP,
    )?;
    if exit != Some(0) {
        return Err(TrustRefusal::Git(format!("config exited with {exit:?}")));
    }
    if truncated {
        return Err(TrustRefusal::TooLarge);
    }
    let mut hash = Sha256::new();
    hash.update(b"dudes.trust.config");
    for entry in out
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
            Some(at) => (&entry[..at], Some(&entry[at + 1..])),
            None => (entry, None),
        };
        let lower = String::from_utf8_lossy(key).to_ascii_lowercase();
        if let Some((_, what)) = REFUSED_KEYS
            .iter()
            .find(|(name, _)| lower == *name || (name.ends_with('.') && lower.starts_with(name)))
        {
            return Err(TrustRefusal::Unsupported(what));
        }
        if lower.starts_with("branch.") || lower.starts_with("remote.") {
            continue;
        }
        hash.update((key.len() as u64).to_be_bytes());
        hash.update(key);
        match value {
            Some(value) => {
                hash.update([1]);
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value);
            }
            None => hash.update([0]),
        }
    }
    Ok(Hex32::new(hash.finalize().into()))
}
