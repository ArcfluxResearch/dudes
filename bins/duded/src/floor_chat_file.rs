//! Files a person sends in Chat: the daemon keeps a copy in the agent's own folder and the message
//! names its path.
//!
//! A browser page hands over a file's bytes and name, never its path, so the bytes come here. The
//! daemon never opens, parses or runs them. Nothing the page sends becomes a folder or a path part:
//! the name is cut down to a short ASCII set and put after random hex in one flat folder.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use dudes_types::AgentId;

/// The largest file one upload may carry.
pub(crate) const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// The most one agent's `files` folder may hold, counting the file being added.
pub(crate) const MAX_FOLDER_BYTES: u64 = 200 * 1024 * 1024;
/// The folder under an agent's directory.
pub(crate) const FOLDER: &str = dudes_providers::adapter::CHAT_FILES_DIR;
const MAX_NAME_BYTES: usize = 80;

static UPLOADING: AtomicBool = AtomicBool::new(false);

/// The one place an upload body is held. Only one exists at a time in the daemon, so the memory
/// the uploads use is one body, not one per connection.
pub(crate) struct UploadSlot(());

impl UploadSlot {
    pub(crate) fn take() -> Option<Self> {
        UPLOADING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(()))
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        UPLOADING.store(false, Ordering::Release);
    }
}

/// A name safe to put in a path: ASCII letters, digits, `.`, `_` and `-` only, no leading or
/// trailing dot, at most 80 bytes, and `file` when nothing is left.
pub(crate) fn clean_name(raw: &[u8]) -> String {
    let kept: String = raw
        .iter()
        .copied()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        .map(char::from)
        .collect();
    let trimmed = kept.trim_matches('.');
    let cut = &trimmed[..trimmed.len().min(MAX_NAME_BYTES)];
    if cut.is_empty() {
        "file".to_owned()
    } else {
        cut.to_owned()
    }
}

/// The percent-decoded bytes of one query member of a request target.
pub(crate) fn query_value(target: &str, key: &str) -> Option<Vec<u8>> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let pair = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[at], pair) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

/// What was kept.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stored {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) bytes: usize,
}

/// Why nothing was kept.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `files` is not a real folder, so nothing is written through it.
    NotAFolder,
    /// The folder would pass its limit; the path lets the person clear it.
    Full(PathBuf),
    /// The disk refused.
    Io,
}

fn hex_id() -> String {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system's entropy source");
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn folder_bytes(folder: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(folder)? {
        let meta = fs::symlink_metadata(entry?.path())?;
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// Keep `bytes` as `<agent_dir>/files/<16 hex>-<clean name>`.
pub(crate) fn store(agent_dir: &Path, raw_name: &[u8], bytes: &[u8]) -> Result<Stored, Refusal> {
    let folder = agent_dir.join(FOLDER);
    match fs::symlink_metadata(&folder) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(Refusal::NotAFolder),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => make_folder(&folder)?,
        Err(_) => return Err(Refusal::Io),
    }
    let held = folder_bytes(&folder).map_err(|_| Refusal::Io)?;
    if held.saturating_add(bytes.len() as u64) > MAX_FOLDER_BYTES {
        return Err(Refusal::Full(folder));
    }
    let name = clean_name(raw_name);
    let id = hex_id();
    let temporary = folder.join(format!(".{id}.part"));
    let path = folder.join(format!("{id}-{name}"));
    let written = create_private(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(Refusal::Io);
    }
    Ok(Stored {
        path,
        name,
        bytes: bytes.len(),
    })
}

/// A freshly hired agent may not have its own directory yet; the mailbox adopts one made here.
fn make_folder(folder: &Path) -> Result<(), Refusal> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(folder).map_err(|_| Refusal::Io)
}

fn create_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Delete an agent's `files` folder, built from its typed id under the agents root. A `files`
/// that is a link loses only the link.
pub(crate) fn remove_for(agents_root: &Path, agent: AgentId) {
    let target = agents_root.join(agent.to_string()).join(FOLDER);
    let Ok(meta) = fs::symlink_metadata(&target) else {
        return;
    };
    let removed = if meta.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    };
    if let Err(error) = removed {
        tracing::warn!(%agent, %error, "a retired agent's files folder could not be deleted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cut_to_a_flat_safe_set() {
        assert_eq!(clean_name(b"../../etc/passwd"), "etcpasswd");
        assert_eq!(
            clean_name("my résumé (final).pdf".as_bytes()),
            "myrsumfinal.pdf"
        );
        assert_eq!(clean_name(b".bashrc"), "bashrc");
        assert_eq!(clean_name(b"notes.txt."), "notes.txt");
        assert_eq!(clean_name(b""), "file");
        assert_eq!(clean_name(b"..."), "file");
        assert_eq!(clean_name(&[b'a'; 200]), "a".repeat(80));
    }

    #[test]
    fn query_values_are_percent_decoded() {
        let target = "/floor/chat/file?agent=AG&name=my%20r%C3%A9sum%C3%A9.pdf";
        assert_eq!(query_value(target, "agent").as_deref(), Some(&b"AG"[..]));
        assert_eq!(
            query_value(target, "name").as_deref(),
            Some("my résumé.pdf".as_bytes())
        );
        assert_eq!(query_value(target, "other"), None);
    }

    #[test]
    fn a_full_folder_refuses_and_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(FOLDER);
        fs::create_dir(&folder).unwrap();
        fs::File::create(folder.join("big"))
            .unwrap()
            .set_len(MAX_FOLDER_BYTES - 3)
            .unwrap();
        assert!(store(dir.path(), b"a", b"12").is_ok());
        assert_eq!(store(dir.path(), b"a", b"12"), Err(Refusal::Full(folder)));
    }
}
