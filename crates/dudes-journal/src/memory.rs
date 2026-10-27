//! Replayable applied-memory projection and its immutable out-of-line body store.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::hash_map::RandomState;
use std::fs;
#[cfg(unix)]
use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
use std::io;
#[cfg(unix)]
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use dudes_proto::{PayloadRef, PeerProvenance, Record};
use dudes_types::{AgentId, Lsn, Timestamp};

mod condense;
mod search;
mod view;
pub use condense::{
    CondensePlan, MemoryCondenseConfig, MemoryCondenseError, PreparedMemoryCondense,
};
pub use search::MemorySearchError;
pub use view::{
    MemoryEntry, MemoryPage, MemorySummary, MemorySummaryKind, Withheld, WithheldAppend,
};

/// The inclusive inline-body threshold.
pub const INLINE_MEMORY_BYTES: usize = 4 * 1024;

#[cfg(unix)]
const MEMORY_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const MEMORY_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What folding one record did to applied memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryApply {
    /// A valid applied append entered the projection.
    Applied,
    /// The record is not an applied memory append.
    Ignored,
    /// An append had competing, missing, misplaced, or altered body bytes.
    Corrupt(MemoryCorruption),
}

/// Why an applied append was not safe to expose to memory, condense, or a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryCorruption {
    /// Inline text and a body reference were both present.
    CompetingBodies,
    /// Inline text exceeded the inline-body threshold.
    InlineOverThreshold,
    /// A reference claimed a body at or below the inline threshold.
    ReferencedAtOrUnderThreshold,
    /// The reference did not name this agent's daemon-owned relative body directory.
    InvalidPath,
    /// The referenced body could not be read.
    MissingBody,
    /// The reference resolved to a symlink, directory, or another non-regular object.
    NonRegularBody,
    /// The referenced regular file is not owned by the daemon's state-tree owner.
    OwnershipMismatch,
    /// The referenced body inode has another hardlink name.
    AliasedBody,
    /// The referenced length differed from the bytes on disk.
    LengthMismatch,
    /// The referenced BLAKE3 differed from the bytes on disk.
    DigestMismatch,
    /// The referenced bytes were not UTF-8.
    InvalidUtf8,
    /// This platform does not yet provide the required secure body-store traversal.
    UnsupportedPlatform,
}

/// A memory record whose out-of-line body guard remains live through journal admission.
#[derive(Debug)]
pub struct PreparedMemoryAppend {
    record: Record,
    #[cfg(unix)]
    guard: Option<BodyGuard>,
}

impl PreparedMemoryAppend {
    /// The record to append while this preparation remains alive.
    #[must_use]
    pub const fn record(&self) -> &Record {
        &self.record
    }

    /// Revalidate the retained body handle and final name before acknowledging admission.
    ///
    /// # Errors
    ///
    /// The body gained an alias, changed ownership, or no longer occupies its validated name.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "the Unix guard performs the fallible revalidation; non-Unix can prepare \
                      only inline records but keeps the same admission interface"
        )
    )]
    pub fn revalidate(&self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(guard) = &self.guard {
            return guard.validate();
        }
        Ok(())
    }
}

impl std::ops::Deref for PreparedMemoryAppend {
    type Target = Record;

    fn deref(&self) -> &Self::Target {
        self.record()
    }
}

/// One applied append: its decoded text, its record's time and the provenance it recorded.
#[derive(Debug)]
struct Applied {
    at: Timestamp,
    provenance: Option<PeerProvenance>,
    text: String,
}

/// Applied memory rebuilt from journal records.
///
/// The body root is configuration, not state. The only entries here are valid `MemoryAppend`
/// records keyed by their journal LSN; quarantine has no bytes and folds as [`MemoryApply::Ignored`],
/// leaving only a count and its newest markers in a map of its own.
#[derive(Debug)]
pub struct MemoryProjection {
    body_root: StateRoot,
    entries: BTreeMap<AgentId, BTreeMap<Lsn, Applied>>,
    withheld: BTreeMap<AgentId, view::WithheldLog>,
    summaries: BTreeMap<AgentId, condense::ActiveSummary>,
    sizes: BTreeMap<AgentId, condense::RawSize>,
    first_nul: BTreeMap<AgentId, (Lsn, usize)>,
    /// The lsn of the latest `TurnIssued` seen for each agent. Tracked here, not on a run-local map, so a model-condense call's own rate
    /// limit is derived from the journal and survives both a restart and a goal change: replay
    /// and a live fold reach it by the identical route, in the identical order, for the identical
    /// reason every other field on this projection does.
    last_turn_issued: BTreeMap<AgentId, Lsn>,
    /// The lsn of the latest `MemoryCondenseModelAdmitted` seen for each agent,
    /// admitted or refused — an attempt is an attempt either way. Paired with
    /// [`Self::last_turn_issued`] by [`Self::model_condense_already_admitted_this_turn`].
    last_condense_model_admitted: BTreeMap<AgentId, Lsn>,
}

impl MemoryProjection {
    /// An empty projection anchored to one validated state-directory handle.
    ///
    /// # Errors
    ///
    /// On Unix, the state directory could not be opened without following links or is not owned
    /// by the daemon's effective user.
    pub fn new(body_root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            body_root: StateRoot::open(body_root.as_ref())?,
            entries: BTreeMap::new(),
            withheld: BTreeMap::new(),
            summaries: BTreeMap::new(),
            sizes: BTreeMap::new(),
            first_nul: BTreeMap::new(),
            last_turn_issued: BTreeMap::new(),
            last_condense_model_admitted: BTreeMap::new(),
        })
    }

    /// Prepare the exact decoded bytes for a durable applied record.
    ///
    /// At or under 4 KiB the bytes remain inline. Above it, this writes and fsyncs one immutable
    /// body and fsyncs its directory before returning the record that may point at it.
    ///
    /// # Errors
    ///
    /// The bytes were not UTF-8, the body directory/file could not be written and synced, or a
    /// pre-existing path with the expected digest did not contain the exact bytes.
    pub fn prepare_append(
        &self,
        agent: AgentId,
        payload: &[u8],
        provenance: PeerProvenance,
    ) -> io::Result<PreparedMemoryAppend> {
        self.prepare_append_with(agent, payload, provenance, &|_, directory| {
            directory.sync_all()
        })
    }

    fn prepare_append_with(
        &self,
        agent: AgentId,
        payload: &[u8],
        provenance: PeerProvenance,
        sync_dir: &impl Fn(SyncPoint, &fs::File) -> io::Result<()>,
    ) -> io::Result<PreparedMemoryAppend> {
        let text = std::str::from_utf8(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if payload.len() <= INLINE_MEMORY_BYTES {
            return Ok(PreparedMemoryAppend {
                record: Record::MemoryAppend {
                    agent,
                    text: text.to_owned(),
                    body: None,
                    provenance: Some(provenance),
                },
                #[cfg(unix)]
                guard: None,
            });
        }

        let hash = blake3::hash(payload).to_hex().to_string();
        let body_name = canonical_body_name(&hash).expect("BLAKE3 hex is one canonical body name");
        let relative = body_path(agent, &hash);
        require_secure_out_of_line_body_store(SECURE_OUT_OF_LINE_BODY_STORE)?;
        let guard = persist_immutable_with(&self.body_root, agent, &body_name, payload, sync_dir)?;
        #[cfg(not(unix))]
        let _ = guard;
        Ok(PreparedMemoryAppend {
            record: Record::MemoryAppend {
                agent,
                text: String::new(),
                body: Some(PayloadRef {
                    path: relative,
                    decoded_len: u64::try_from(payload.len()).expect("usize fits into u64"),
                    blake3: hash,
                }),
                provenance: Some(provenance),
            },
            #[cfg(unix)]
            guard: Some(guard),
        })
    }

    /// Fold one record at its authoritative journal LSN. A record folded without its entry has no
    /// time, so it is labelled [`Timestamp::UNIX_EPOCH`]; the daemon folds through
    /// [`MemoryProjection::apply_entry`].
    pub fn apply(&mut self, lsn: Lsn, record: &Record) -> MemoryApply {
        self.fold(lsn, Timestamp::UNIX_EPOCH, record)
            .unwrap_or(MemoryApply::Ignored)
    }

    fn fold_plain(&mut self, lsn: Lsn, at: Timestamp, record: &Record) -> MemoryApply {
        // Neither changes active history; both exist only to feed the model-condense rate limit
        // below, fed from this same fold so a restart or a goal change rebuilds the
        // identical answer a live run already had.
        if let Some(issue) = record.ordinary_issue() {
            self.last_turn_issued.insert(issue.agent, lsn);
            return MemoryApply::Ignored;
        }
        if let Record::MemoryCondenseModelAdmitted { agent, .. } = record {
            self.last_condense_model_admitted.insert(*agent, lsn);
            return MemoryApply::Ignored;
        }
        if let Record::MemoryAppendQuarantined {
            agent,
            payload,
            provenance,
            reason,
        } = record
        {
            self.withheld.entry(*agent).or_default().fold(
                lsn,
                at,
                payload.decoded_len,
                *provenance,
                reason,
            );
            return MemoryApply::Ignored;
        }
        let Record::MemoryAppend {
            agent,
            text,
            body,
            provenance,
        } = record
        else {
            return MemoryApply::Ignored;
        };

        if self
            .entries
            .get(agent)
            .is_some_and(|entries| entries.contains_key(&lsn))
        {
            return MemoryApply::Ignored;
        }

        let resolved = match body {
            None if text.len() <= INLINE_MEMORY_BYTES => text.clone(),
            None => return MemoryApply::Corrupt(MemoryCorruption::InlineOverThreshold),
            Some(_) if !text.is_empty() => {
                return MemoryApply::Corrupt(MemoryCorruption::CompetingBodies);
            }
            Some(reference) => match self.resolve(*agent, reference) {
                Ok(text) => text,
                Err(reason) => return MemoryApply::Corrupt(reason),
            },
        };

        self.track_append(*agent, lsn, &resolved);
        self.entries.entry(*agent).or_default().insert(
            lsn,
            Applied {
                at,
                provenance: *provenance,
                text: resolved,
            },
        );
        MemoryApply::Applied
    }

    /// The current textual digest gate 7 prepends to the next goal task.
    #[must_use]
    pub fn digest(&self, agent: AgentId) -> String {
        if let Some(summary) = self.summaries.get(&agent) {
            let mut digest = summary.rendered.clone();
            if let Some(entries) = self.entries.get(&agent) {
                for (_, applied) in entries.range((
                    std::ops::Bound::Excluded(summary.envelope.to_lsn),
                    std::ops::Bound::Unbounded,
                )) {
                    digest.push('\n');
                    digest.push_str(&applied.text);
                }
            }
            return digest;
        }
        join(
            self.entries
                .get(&agent)
                .into_iter()
                .flat_map(BTreeMap::values)
                .map(|applied| &applied.text),
        )
    }

    /// Applied bodies in one inclusive LSN range, for a condense task.
    #[must_use]
    pub fn condense_input(&self, agent: AgentId, from: Lsn, to: Lsn) -> String {
        self.entries
            .get(&agent)
            .map_or_else(String::new, |entries| {
                join(entries.range(from..=to).map(|(_, applied)| &applied.text))
            })
    }

    /// Whole applied entries after `since_lsn`, in LSN order, bounded by decoded UTF-8 bytes.
    /// Empty pages have LSN zero. A page's LSN is its last included append.
    ///
    /// # Errors
    /// Returns the first entry's byte length if it exceeds the page bound by itself.
    pub fn read(
        &self,
        agent: AgentId,
        since_lsn: Option<Lsn>,
        max_bytes: usize,
    ) -> Result<(String, Lsn), usize> {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut text = String::new();
        let mut last = Lsn(0);
        if let Some(entries) = self.entries.get(&agent) {
            let start = since_lsn.map_or(Unbounded, Excluded);
            for (lsn, Applied { text: body, .. }) in entries.range((start, Unbounded)) {
                let separator = usize::from(last != Lsn(0));
                if text
                    .len()
                    .saturating_add(separator)
                    .saturating_add(body.len())
                    > max_bytes
                {
                    if last == Lsn(0) {
                        return Err(body.len());
                    }
                    break;
                }
                if separator > 0 {
                    text.push('\n');
                }
                text.push_str(body);
                last = *lsn;
            }
        }
        Ok((text, last))
    }

    fn resolve(&self, agent: AgentId, reference: &PayloadRef) -> Result<String, MemoryCorruption> {
        if reference.decoded_len <= INLINE_MEMORY_BYTES as u64 {
            return Err(MemoryCorruption::ReferencedAtOrUnderThreshold);
        }
        let Some(body_name) = validated_body_name(agent, &reference.path, &reference.blake3) else {
            return Err(MemoryCorruption::InvalidPath);
        };
        if !SECURE_OUT_OF_LINE_BODY_STORE {
            return Err(MemoryCorruption::UnsupportedPlatform);
        }
        let bytes = read_body_relative(&self.body_root, agent, &body_name, reference.decoded_len)?;
        if blake3::hash(&bytes).to_hex().as_str() != reference.blake3 {
            return Err(MemoryCorruption::DigestMismatch);
        }
        String::from_utf8(bytes).map_err(|_| MemoryCorruption::InvalidUtf8)
    }
}

fn join<'a>(values: impl Iterator<Item = &'a String>) -> String {
    let mut values = values.peekable();
    let mut joined = String::new();
    while let Some(value) = values.next() {
        joined.push_str(value);
        if values.peek().is_some() {
            joined.push('\n');
        }
    }
    joined
}

fn body_path(agent: AgentId, hash: &str) -> PathBuf {
    PathBuf::from("agents")
        .join(agent.to_string())
        .join("memory")
        .join(format!("{hash}.body"))
}

fn canonical_body_name(hash: &str) -> Option<String> {
    (hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| format!("{hash}.body"))
}

fn validated_body_name(agent: AgentId, path: &Path, hash: &str) -> Option<String> {
    let name = canonical_body_name(hash)?;
    (path == body_path(agent, hash)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_))))
    .then_some(name)
}

#[cfg(unix)]
const SECURE_OUT_OF_LINE_BODY_STORE: bool = true;
#[cfg(not(unix))]
const SECURE_OUT_OF_LINE_BODY_STORE: bool = false;

fn require_secure_out_of_line_body_store(supported: bool) -> io::Result<()> {
    if supported {
        Ok(())
    } else {
        Err(unsupported_body_store_error())
    }
}

fn unsupported_body_store_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "out-of-line memory bodies require secure descriptor-relative traversal",
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(unix)]
enum SyncPoint {
    Memory,
    Agent,
    Agents,
    Root,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(not(unix))]
enum SyncPoint {}

#[derive(Debug)]
struct StateRoot {
    #[cfg(unix)]
    directory: fs::File,
    #[cfg(unix)]
    owner: u32,
    #[cfg(not(unix))]
    _path: PathBuf,
}

#[cfg(unix)]
#[derive(Debug)]
struct BodyGuard {
    root: fs::File,
    agent: AgentId,
    agents: fs::File,
    agent_directory: fs::File,
    directory: fs::File,
    body: fs::File,
    body_name: String,
    owner: u32,
    expected_len: u64,
    expected_blake3: blake3::Hash,
}

#[cfg(unix)]
impl BodyGuard {
    fn validate(&self) -> io::Result<()> {
        let agents = open_directory_at(&self.root, "agents", self.owner, false)?;
        same_inode(&agents, &self.agents)?;
        let agent = open_directory_at(&agents, &self.agent.to_string(), self.owner, false)?;
        same_inode(&agent, &self.agent_directory)?;
        let memory = open_directory_at(&agent, "memory", self.owner, false)?;
        same_inode(&memory, &self.directory)?;
        validate_final_body(&self.directory, &self.body_name, &self.body, self.owner)?;
        let bytes = read_bounded_from_start(&self.body, self.expected_len)?;
        if blake3::hash(&bytes) == self.expected_blake3 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "memory body content changed after preparation",
            ))
        }
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
enum BodyGuard {}

impl StateRoot {
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "Unix validates and opens the lifetime root here; non-Unix keeps the same \
                      constructor interface while out-of-line persistence remains fail-closed"
        )
    )]
    fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let directory = open_root(path)?;
            let owner = rustix::process::geteuid().as_raw();
            validate_directory(&directory.metadata()?, owner)?;
            Ok(Self { directory, owner })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                _path: path.to_path_buf(),
            })
        }
    }
}

#[cfg(unix)]
struct BodyTree<'a> {
    root: &'a fs::File,
    agents: fs::File,
    agent: fs::File,
    memory: fs::File,
    owner: u32,
}

#[cfg(unix)]
impl<'a> BodyTree<'a> {
    fn open(root: &'a StateRoot, agent: AgentId, create: bool) -> io::Result<Self> {
        let owner = root.owner;
        let open_child = |parent: &fs::File, name: &str| {
            if create {
                open_or_create_directory_at(parent, name, owner)
            } else {
                open_directory_at(parent, name, owner, true)
            }
        };
        let agents = open_child(&root.directory, "agents")?;
        let agent = open_child(&agents, &agent.to_string())?;
        let memory = open_child(&agent, "memory")?;
        Ok(Self {
            root: &root.directory,
            agents,
            agent,
            memory,
            owner,
        })
    }

    fn sync_all(
        &self,
        sync_dir: &impl Fn(SyncPoint, &fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        for (point, directory) in [
            (SyncPoint::Memory, &self.memory),
            (SyncPoint::Agent, &self.agent),
            (SyncPoint::Agents, &self.agents),
            (SyncPoint::Root, self.root),
        ] {
            sync_dir(point, directory)?;
        }
        Ok(())
    }

    fn validate_named_chain(&self, agent: AgentId) -> io::Result<()> {
        let agents = open_directory_at(self.root, "agents", self.owner, false)?;
        same_inode(&agents, &self.agents)?;
        let agent_dir = open_directory_at(&agents, &agent.to_string(), self.owner, false)?;
        same_inode(&agent_dir, &self.agent)?;
        let memory = open_directory_at(&agent_dir, "memory", self.owner, false)?;
        same_inode(&memory, &self.memory)
    }
}

#[cfg(unix)]
fn persist_immutable_with(
    root: &StateRoot,
    agent: AgentId,
    body_name: &str,
    payload: &[u8],
    sync_dir: &impl Fn(SyncPoint, &fs::File) -> io::Result<()>,
) -> io::Result<BodyGuard> {
    let tree = BodyTree::open(root, agent, true)?;
    let body = match open_regular_at(&tree.memory, body_name, tree.owner) {
        Ok(mut file) => {
            verify_existing(&mut file, payload, true)?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            persist_new(&tree.memory, body_name, payload, tree.owner)?
        }
        Err(error) => return Err(error),
    };

    // Every successful attempt discharges the whole bottom-up durability chain. This is deliberate
    // even for a reused body: a previous attempt may have failed after creating an ancestor.
    tree.sync_all(sync_dir)?;
    tree.validate_named_chain(agent)?;
    validate_final_body(&tree.memory, body_name, &body, tree.owner)?;
    Ok(BodyGuard {
        root: tree.root.try_clone()?,
        agent,
        agents: tree.agents,
        agent_directory: tree.agent,
        directory: tree.memory,
        body,
        body_name: body_name.to_owned(),
        owner: tree.owner,
        expected_len: u64::try_from(payload.len()).expect("usize fits into u64"),
        expected_blake3: blake3::hash(payload),
    })
}

#[cfg(not(unix))]
fn persist_immutable_with(
    _root: &StateRoot,
    _agent: AgentId,
    _hash: &str,
    _payload: &[u8],
    _sync_dir: &impl Fn(SyncPoint, &fs::File) -> io::Result<()>,
) -> io::Result<BodyGuard> {
    Err(unsupported_body_store_error())
}

#[cfg(unix)]
fn open_root(path: &Path) -> io::Result<fs::File> {
    open_at(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
}

#[cfg(unix)]
fn open_at(
    parent: impl std::os::fd::AsFd,
    path: &Path,
    flags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> io::Result<fs::File> {
    rustix::fs::openat(parent, path, flags, mode)
        .map(fs::File::from)
        .map_err(Into::into)
}

#[cfg(unix)]
fn open_or_create_directory_at(parent: &fs::File, name: &str, owner: u32) -> io::Result<fs::File> {
    match open_directory_at(parent, name, owner, true) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
                Ok(()) => {}
                Err(error) => {
                    let error = io::Error::from(error);
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
            }
            open_directory_at(parent, name, owner, true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_directory_at(
    parent: &fs::File,
    name: &str,
    owner: u32,
    narrow: bool,
) -> io::Result<fs::File> {
    let directory = open_at(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    validate_directory(&directory.metadata()?, owner)?;
    if narrow {
        restrict_descriptor(&directory, MEMORY_DIR_MODE)?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_regular_at(parent: &fs::File, name: &str, owner: u32) -> io::Result<fs::File> {
    let file = open_at(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    validate_regular(&file.metadata()?, owner)?;
    Ok(file)
}

#[cfg(unix)]
fn persist_new(
    parent: &fs::File,
    body_name: &str,
    payload: &[u8],
    owner: u32,
) -> io::Result<fs::File> {
    let (stage_name, mut stage_file) = create_stage(parent, owner)?;
    let outcome = (|| {
        stage_file.write_all(payload)?;
        stage_file.sync_all()?;
        drop(stage_file);
        let linked = match rustix::fs::linkat(
            parent,
            stage_name.as_str(),
            parent,
            body_name,
            rustix::fs::AtFlags::empty(),
        ) {
            Ok(()) => true,
            Err(error) => {
                let error = io::Error::from(error);
                if error.kind() == io::ErrorKind::AlreadyExists {
                    false
                } else {
                    return Err(error);
                }
            }
        };
        unlink_at(parent, &stage_name)?;
        let mut body = open_regular_at(parent, body_name, owner)?;
        verify_existing(&mut body, payload, !linked)?;
        Ok(body)
    })();
    if outcome.is_err() {
        let _ = unlink_at(parent, &stage_name);
    }
    outcome
}

#[cfg(unix)]
fn create_stage(parent: &fs::File, owner: u32) -> io::Result<(String, fs::File)> {
    for _ in 0..128 {
        let name = format!(".memory-{:016x}.stage", stage_nonce());
        match open_at(
            parent,
            Path::new(&name),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ) {
            Ok(file) => {
                validate_regular(&file.metadata()?, owner)?;
                restrict_descriptor(&file, MEMORY_FILE_MODE)?;
                return Ok((name, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate an exclusive memory staging file",
    ))
}

#[cfg(unix)]
fn unlink_at(parent: &fs::File, name: &str) -> io::Result<()> {
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty()).map_err(Into::into)
}

#[cfg(unix)]
fn stage_nonce() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    std::process::id().hash(&mut hasher);
    STAGE_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(unix)]
fn verify_existing(file: &mut fs::File, payload: &[u8], narrow: bool) -> io::Result<()> {
    if narrow {
        restrict_descriptor(file, MEMORY_FILE_MODE)?;
    }
    let existing = read_bounded(
        file,
        u64::try_from(payload.len()).expect("usize fits into u64"),
    )?;
    if existing != payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing immutable memory body differs",
        ));
    }
    file.sync_all()
}

#[cfg(unix)]
fn validate_final_body(
    parent: &fs::File,
    body_name: &str,
    body: &fs::File,
    owner: u32,
) -> io::Result<()> {
    restrict_descriptor(body, MEMORY_FILE_MODE)?;
    validate_regular(&body.metadata()?, owner)?;
    let named = open_regular_at(parent, body_name, owner)?;
    same_inode(&named, body)?;
    restrict_descriptor(&named, MEMORY_FILE_MODE)?;
    validate_regular(&named.metadata()?, owner)
}

#[cfg(unix)]
fn validate_directory(metadata: &fs::Metadata, owner: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory path is not a directory",
        ));
    }
    if metadata.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "memory directory is not owned by the state-tree owner",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_regular(metadata: &fs::Metadata, owner: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory path is not a regular file",
        ));
    }
    if metadata.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "memory body is not owned by the state-tree owner",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory body has more than one hardlink name",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_inode(left: &fs::File, right: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let left = left.metadata()?;
    let right = right.metadata()?;
    if left.dev() == right.dev() && left.ino() == right.ino() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory directory changed during descriptor-relative traversal",
        ))
    }
}

#[cfg(unix)]
fn read_body_relative(
    root: &StateRoot,
    agent: AgentId,
    body_name: &str,
    expected_len: u64,
) -> Result<Vec<u8>, MemoryCorruption> {
    let tree = BodyTree::open(root, agent, false).map_err(|error| map_body_io_error(&error))?;
    let mut file = open_regular_for_projection(&tree.memory, body_name, tree.owner)?;
    let bytes = read_bounded(&mut file, expected_len).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            MemoryCorruption::LengthMismatch
        } else {
            MemoryCorruption::MissingBody
        }
    })?;
    tree.validate_named_chain(agent)
        .map_err(|error| map_body_io_error(&error))?;
    validate_final_body(&tree.memory, body_name, &file, tree.owner)
        .map_err(|error| map_body_io_error(&error))?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_body_relative(
    _root: &StateRoot,
    _agent: AgentId,
    _body_name: &str,
    _expected_len: u64,
) -> Result<Vec<u8>, MemoryCorruption> {
    Err(MemoryCorruption::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_regular_for_projection(
    parent: &fs::File,
    name: &str,
    owner: u32,
) -> Result<fs::File, MemoryCorruption> {
    use std::os::unix::fs::MetadataExt as _;

    let file = rustix::fs::openat(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP {
            MemoryCorruption::NonRegularBody
        } else {
            MemoryCorruption::MissingBody
        }
    })?;
    let metadata = file.metadata().map_err(|_| MemoryCorruption::MissingBody)?;
    if !metadata.file_type().is_file() {
        return Err(MemoryCorruption::NonRegularBody);
    }
    if metadata.uid() != owner {
        return Err(MemoryCorruption::OwnershipMismatch);
    }
    if metadata.nlink() != 1 {
        return Err(MemoryCorruption::AliasedBody);
    }
    restrict_descriptor(&file, MEMORY_FILE_MODE)
        .map_err(|_| MemoryCorruption::OwnershipMismatch)?;
    let metadata = file.metadata().map_err(|_| MemoryCorruption::MissingBody)?;
    if metadata.uid() != owner {
        return Err(MemoryCorruption::OwnershipMismatch);
    }
    if metadata.nlink() != 1 {
        return Err(MemoryCorruption::AliasedBody);
    }
    Ok(file)
}

#[cfg(unix)]
fn map_body_io_error(error: &io::Error) -> MemoryCorruption {
    match error.kind() {
        io::ErrorKind::PermissionDenied => MemoryCorruption::OwnershipMismatch,
        io::ErrorKind::InvalidData | io::ErrorKind::NotADirectory => {
            MemoryCorruption::NonRegularBody
        }
        _ => MemoryCorruption::MissingBody,
    }
}

#[cfg(unix)]
fn read_bounded(file: &mut fs::File, expected_len: u64) -> io::Result<Vec<u8>> {
    if file.metadata()?.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory body length differs",
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(file)
        .take(expected_len.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).expect("usize fits into u64") != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory body length changed while reading",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_bounded_from_start(file: &fs::File, expected_len: u64) -> io::Result<Vec<u8>> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    read_bounded(&mut file, expected_len)
}

#[cfg(unix)]
fn restrict_descriptor(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::cell::RefCell;

    use super::*;

    fn agent() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ULID")
    }

    fn projection(path: PathBuf) -> MemoryProjection {
        MemoryProjection::new(path).expect("open the memory state root")
    }

    #[cfg(unix)]
    fn expected_syncs() -> Vec<SyncPoint> {
        vec![
            SyncPoint::Memory,
            SyncPoint::Agent,
            SyncPoint::Agents,
            SyncPoint::Root,
        ]
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_body_tree_syncs_bottom_up_before_a_record_can_be_returned() {
        let root = tempfile::tempdir().expect("a body root");
        let projection = projection(root.path().to_path_buf());
        let observed = RefCell::new(Vec::new());
        let record = projection
            .prepare_append_with(
                agent(),
                &vec![b'd'; INLINE_MEMORY_BYTES + 1],
                PeerProvenance::Agent(agent()),
                &|point, directory| {
                    observed.borrow_mut().push(point);
                    directory.sync_all()
                },
            )
            .expect("every directory sync completed before the record returned");

        assert!(matches!(
            record.record(),
            Record::MemoryAppend { body: Some(_), .. }
        ));
        assert_eq!(*observed.borrow(), expected_syncs());
    }

    #[cfg(unix)]
    #[test]
    fn any_directory_sync_failure_withholds_the_referencing_record() {
        for fail_at in 0..4 {
            let root = tempfile::tempdir().expect("a body root");
            let projection = projection(root.path().to_path_buf());
            let observed = RefCell::new(Vec::new());
            let result = projection.prepare_append_with(
                agent(),
                &vec![b'f'; INLINE_MEMORY_BYTES + 1],
                PeerProvenance::Agent(agent()),
                &|point, directory| {
                    let mut observed = observed.borrow_mut();
                    observed.push(point);
                    if observed.len() == fail_at + 1 {
                        Err(io::Error::other("injected directory sync failure"))
                    } else {
                        directory.sync_all()
                    }
                },
            );
            assert!(
                result.is_err(),
                "failure {fail_at} returned a body reference"
            );
            assert_eq!(
                *observed.borrow(),
                expected_syncs()[..=fail_at],
                "the sync sequence stopped at injected failure {fail_at}"
            );

            observed.borrow_mut().clear();
            projection
                .prepare_append_with(
                    agent(),
                    &vec![b'f'; INLINE_MEMORY_BYTES + 1],
                    PeerProvenance::Agent(agent()),
                    &|point, directory| {
                        observed.borrow_mut().push(point);
                        directory.sync_all()
                    },
                )
                .expect("a retry completes every previously owed directory sync");
            assert_eq!(
                *observed.borrow(),
                expected_syncs(),
                "retry after failure {fail_at} syncs the complete chain"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacing_an_opened_parent_cannot_redirect_or_publish_a_body() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("a body root");
        let outside = tempfile::tempdir().expect("an outside tree");
        fs::create_dir(outside.path().join("memory")).expect("an attacker memory directory");
        let projection = projection(root.path().to_path_buf());
        let agent_dir = root.path().join("agents").join(agent().to_string());
        let parked = root.path().join("parked-agent");
        let raced = std::cell::Cell::new(false);

        let result = projection.prepare_append_with(
            agent(),
            &vec![b'r'; INLINE_MEMORY_BYTES + 1],
            PeerProvenance::Agent(agent()),
            &|_, directory| {
                if !raced.replace(true) {
                    fs::rename(&agent_dir, &parked).expect("replace the opened parent");
                    symlink(outside.path(), &agent_dir).expect("redirect the original name");
                }
                directory.sync_all()
            },
        );

        assert!(
            result.is_err(),
            "a body reference is withheld when an opened ancestor is replaced"
        );
        assert!(
            fs::read_dir(outside.path().join("memory"))
                .expect("outside memory remains readable")
                .next()
                .is_none(),
            "no body operation followed the replacement into the outside tree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_body_is_revalidated_after_the_directory_sync_window() {
        use std::os::unix::fs::PermissionsExt as _;

        let payload = vec![b'g'; INLINE_MEMORY_BYTES + 1];
        let hash = blake3::hash(&payload).to_hex().to_string();

        let hardlink_root = tempfile::tempdir().expect("a hardlink root");
        let hardlink_projection = projection(hardlink_root.path().to_path_buf());
        let body = hardlink_root.path().join(body_path(agent(), &hash));
        let alias = hardlink_root.path().join("late-body-alias");
        let raced = std::cell::Cell::new(false);
        let hardlink = hardlink_projection.prepare_append_with(
            agent(),
            &payload,
            PeerProvenance::Agent(agent()),
            &|_, directory| {
                if !raced.replace(true) {
                    fs::hard_link(&body, &alias).expect("add a link during directory sync");
                }
                directory.sync_all()
            },
        );
        assert!(
            hardlink.is_err(),
            "a late hardlink withholds the body reference"
        );

        let replacement_root = tempfile::tempdir().expect("a replacement root");
        let replacement_projection = projection(replacement_root.path().to_path_buf());
        let body = replacement_root.path().join(body_path(agent(), &hash));
        let parked = replacement_root.path().join("parked-body");
        let raced = std::cell::Cell::new(false);
        let replacement = replacement_projection.prepare_append_with(
            agent(),
            &payload,
            PeerProvenance::Agent(agent()),
            &|_, directory| {
                if !raced.replace(true) {
                    fs::rename(&body, &parked).expect("move the validated body name");
                    fs::write(&body, &payload).expect("replace it with equal bytes");
                    fs::set_permissions(&body, fs::Permissions::from_mode(MEMORY_FILE_MODE))
                        .expect("give the replacement the expected mode");
                }
                directory.sync_all()
            },
        );
        assert!(
            replacement.is_err(),
            "an equal-byte final-name replacement withholds the body reference"
        );
    }

    #[test]
    fn unsupported_out_of_line_body_store_is_fail_closed() {
        let error = require_secure_out_of_line_body_store(false)
            .expect_err("an unsupported security boundary is never a no-op");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_rejects_out_of_line_prepare_and_replay() {
        let root = tempfile::tempdir().expect("a body root");
        let projection = projection(root.path().to_path_buf());
        let bytes = vec![b'n'; INLINE_MEMORY_BYTES + 1];
        let error = projection
            .prepare_append(agent(), &bytes, PeerProvenance::Agent(agent()))
            .expect_err("non-Unix persistence fails closed");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);

        let mut replay = projection;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let reference = PayloadRef {
            path: body_path(agent(), &hash),
            decoded_len: bytes.len() as u64,
            blake3: hash,
        };
        assert_eq!(
            replay.apply(
                Lsn(1),
                &Record::MemoryAppend {
                    agent: agent(),
                    text: String::new(),
                    body: Some(reference),
                    provenance: Some(PeerProvenance::Agent(agent())),
                }
            ),
            MemoryApply::Corrupt(MemoryCorruption::UnsupportedPlatform)
        );
    }
}
