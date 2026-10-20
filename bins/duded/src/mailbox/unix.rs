use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Read as _, Write as _};
use std::ops::ControlFlow;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use super::index::{Address, Route, SendPlan};
use dudes_proto::Entry;

use rustix::fs::{AtFlags, Dir, Mode, OFlags, RenameFlags};

use super::{
    AgentId, BTreeMap, BTreeSet, Deserialize, INBOX_CAP, Item, Lsn, MailAct, MailFrom, MailId,
    MailState, MailTo, Mailbox, MailboxProblem, RECIPIENTS_PER_PASS, Record, Row, ThreadId,
    Timestamp,
};

const MARKER_BYTES: u64 = 64 * 1024;
const OUTBOX_BYTES: u64 = 1024 * 1024;
const OUTBOX_SCAN: usize = 32;
const INLINE_BYTES: usize = 4096;
/// Raw entries one canonical-name search may read beyond the ones the journal accounts for.
const SCAN_CAP: usize = 1024;
/// One pass stops starting work once it has read this many directory entries, visited this many
/// items or issued this many fsyncs. The pass shares the loop with hook replies, which give up
/// after two seconds, and an fsync can take several milliseconds.
const PASS_ENTRIES: usize = 32 * 1024;
const PASS_ITEMS: usize = 128;
const PASS_FSYNCS: usize = 16;

/// The work one pass has done so far.
#[derive(Debug, Default)]
pub(super) struct Budget {
    entries: Cell<usize>,
    items: Cell<usize>,
    fsyncs: Cell<usize>,
}

impl Budget {
    fn spent(&self) -> bool {
        self.entries.get() >= PASS_ENTRIES
            || self.items.get() >= PASS_ITEMS
            || self.fsyncs.get() >= PASS_FSYNCS
    }

    /// Starts one item of a phase. A spent pass ends a phase only after its first item, so a
    /// recipient whose recorded history alone fills the budget still makes progress each visit.
    fn item(&self, started: &mut bool) -> bool {
        if *started && self.spent() {
            return false;
        }
        *started = true;
        self.items.set(self.items.get() + 1);
        true
    }

    fn sync(&self, file: &File) -> io::Result<()> {
        self.fsyncs.set(self.fsyncs.get() + 1);
        file.sync_all()
    }
}

/// Why an item or a recipient waits for a later pass.
#[derive(Debug)]
enum Defer {
    /// A directory holds more entries than a search may read.
    ScanOverflow(String),
    /// The agent acknowledged the item before it was published again.
    Acknowledged,
}

impl std::fmt::Display for Defer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScanOverflow(directory) => write!(f, "{directory} exceeds its scan cap"),
            Self::Acknowledged => f.write_str("acknowledged before publication"),
        }
    }
}

impl std::error::Error for Defer {}

fn deferral(error: &io::Error) -> Option<&Defer> {
    error.get_ref()?.downcast_ref()
}

/// A directory handle that knows how much of a search it may read and which pass pays for it.
struct Directory {
    file: File,
    label: String,
    cap: usize,
    budget: Rc<Budget>,
}

impl AsFd for Directory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Directory {
    fn child(&self, file: File, label: &str) -> Self {
        Self {
            file,
            label: label.to_owned(),
            cap: self.cap,
            budget: Rc::clone(&self.budget),
        }
    }

    fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    fn sync(&self) -> io::Result<()> {
        self.budget.sync(&self.file)
    }
}

/// How a pass opens one recipient's directories.
struct Opening {
    budget: Rc<Budget>,
    cap: usize,
    /// Only a hired agent's directories are created; an unknown recipient gets none.
    create: bool,
    /// How many times this run has visited the recipient.
    turn: usize,
}

#[cfg(test)]
impl Default for Opening {
    fn default() -> Self {
        Self {
            budget: Rc::default(),
            cap: SCAN_CAP,
            create: true,
            turn: 0,
        }
    }
}

struct Tree {
    root: Directory,
    agents: Directory,
    agent: Directory,
    inbox: Directory,
    done: Directory,
    staging: Directory,
    outbox: Directory,
    id: AgentId,
    root_path: PathBuf,
    budget: Rc<Budget>,
    turn: usize,
}

fn open(parent: impl AsFd, name: impl rustix::path::Arg, flags: OFlags) -> io::Result<File> {
    rustix::fs::openat(
        parent,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(Into::into)
}

fn existing_directory(parent: &Directory, name: &str) -> io::Result<Directory> {
    if !named_canonical(parent, OsStr::new(name))? {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    Ok(parent.child(
        open(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?,
        name,
    ))
}

fn directory(parent: &Directory, name: &str, create: bool) -> io::Result<Directory> {
    if named_canonical(parent, OsStr::new(name))? {
        return Ok(parent.child(
            open(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?,
            name,
        ));
    }
    if !create {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    rustix::fs::mkdirat(parent, name, Mode::RWXU)?;
    parent.sync()?;
    existing_directory(parent, name)
}

fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino() && a.file_type() == b.file_type()
}

#[derive(Debug)]
pub(super) struct OutboxIdentity {
    root: PathBuf,
    agent: AgentId,
    retained: [File; 4],
}

impl OutboxIdentity {
    pub(super) fn check(&self, granted: &Path) -> io::Result<()> {
        let named = self
            .root
            .join("agents")
            .join(self.agent.to_string())
            .join("outbox");
        crate::codex_profile::protected_ancestors(&named)?;
        if named.canonicalize()? != granted {
            return Err(io::Error::other("owned outbox canonical path changed"));
        }
        let tree = Tree::open(&self.root, self.agent, grant_opening())?;
        private_mail_directory(&tree.agent.metadata()?)?;
        private_mail_directory(&tree.outbox.metadata()?)?;
        for (actual, retained) in [
            &tree.root.file,
            &tree.agents.file,
            &tree.agent.file,
            &tree.outbox.file,
        ]
        .into_iter()
        .zip(&self.retained)
        {
            if !same(&actual.metadata()?, &retained.metadata()?) {
                return Err(io::Error::other("owned directory identity changed"));
            }
        }
        Ok(())
    }
}

fn private_mail_directory(metadata: &Metadata) -> io::Result<()> {
    if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "owned directory is not private to this user",
        ));
    }
    Ok(())
}

fn grant_opening() -> Opening {
    Opening {
        budget: Rc::default(),
        cap: SCAN_CAP,
        create: false,
        turn: 0,
    }
}

#[cfg(test)]
pub(super) fn outbox_fixture(root: &Path, agent: AgentId) -> Arc<super::OutboxGrant> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new().mode(0o700).create(root).unwrap();
    let tree = Tree::open(
        root,
        agent,
        Opening {
            create: true,
            ..grant_opening()
        },
    )
    .unwrap();
    tree.outbox_grant().unwrap()
}

impl Mailbox {
    pub(super) fn unix_admit_outbox(&self, agent: AgentId) -> io::Result<Arc<super::OutboxGrant>> {
        let named = self
            .root
            .join("agents")
            .join(agent.to_string())
            .join("outbox");
        let admitted = (|| {
            if !self.live_agents.contains(&agent) || !self.codex_agents.contains(&agent) {
                return Err(io::Error::other(
                    "outbox does not belong to a live Codex hire",
                ));
            }
            if let Some(grant) = self.outboxes.borrow().get(&agent).cloned() {
                grant.identity.check(&grant.path)?;
                return Ok(grant);
            }
            crate::codex_profile::normalized_absolute(&self.root)?;
            crate::codex_profile::protected_ancestors(&named)?;
            let tree = Tree::open(
                &self.root,
                agent,
                Opening {
                    create: true,
                    ..grant_opening()
                },
            )?;
            self.remember_outbox(&tree)?;
            self.outboxes
                .borrow()
                .get(&agent)
                .cloned()
                .ok_or_else(|| io::Error::other("owned mailbox setup did not admit an outbox"))
        })();
        admitted.map_err(|error: io::Error| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Codex outbox {} was refused: {error}", named.display()),
            )
        })
    }

    fn remember_outbox(&self, tree: &Tree) -> io::Result<()> {
        if !self.codex_agents.contains(&tree.id) || self.outboxes.borrow().contains_key(&tree.id) {
            return Ok(());
        }
        self.outboxes
            .borrow_mut()
            .insert(tree.id, tree.outbox_grant()?);
        Ok(())
    }
}

impl Tree {
    fn open(root: &Path, id: AgentId, opening: Opening) -> io::Result<Self> {
        let root_path = root.to_path_buf();
        let root = Directory {
            file: open(rustix::fs::CWD, root, OFlags::RDONLY | OFlags::DIRECTORY)?,
            label: ".".to_owned(),
            cap: opening.cap,
            budget: Rc::clone(&opening.budget),
        };
        let agents = directory(&root, "agents", opening.create)?;
        let agent = directory(&agents, &id.to_string(), opening.create)?;
        let inbox = directory(&agent, "inbox", opening.create)?;
        let done = directory(&inbox, ".done", opening.create)?;
        let staging = directory(&inbox, ".staging", opening.create)?;
        let outbox = directory(&agent, "outbox", opening.create)?;
        Ok(Self {
            root,
            agents,
            agent,
            inbox,
            done,
            staging,
            outbox,
            id,
            root_path,
            budget: opening.budget,
            turn: opening.turn,
        })
    }

    fn outbox_grant(&self) -> io::Result<Arc<super::OutboxGrant>> {
        private_mail_directory(&self.agent.metadata()?)?;
        private_mail_directory(&self.outbox.metadata()?)?;
        let named = self
            .root_path
            .join("agents")
            .join(self.id.to_string())
            .join("outbox");
        crate::codex_profile::protected_ancestors(&named)?;
        let expected = self
            .root_path
            .canonicalize()?
            .join("agents")
            .join(self.id.to_string())
            .join("outbox");
        if named.canonicalize()? != expected {
            return Err(io::Error::other("owned outbox aliases another directory"));
        }
        let grant = super::OutboxGrant {
            path: expected,
            identity: OutboxIdentity {
                root: self.root_path.clone(),
                agent: self.id,
                retained: [
                    self.root.file.try_clone()?,
                    self.agents.file.try_clone()?,
                    self.agent.file.try_clone()?,
                    self.outbox.file.try_clone()?,
                ],
            },
        };
        grant.identity.check(&grant.path)?;
        Ok(Arc::new(grant))
    }

    /// Once the pass is spent a phase starts only one item, so each visit starts it at a
    /// different one and an item that stays stuck cannot keep that place.
    fn order<T>(&self, items: &mut [T]) {
        if self.budget.spent() && !items.is_empty() {
            items.rotate_left(self.turn % items.len());
        }
    }

    fn validate(&self) -> io::Result<()> {
        let named_root = open(
            rustix::fs::CWD,
            self.root_path.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY,
        )?;
        if !same(&named_root.metadata()?, &self.root.metadata()?) {
            return Err(io::Error::other("mailbox root identity changed"));
        }
        let agents = existing_directory(&self.root, "agents")?;
        let agent = existing_directory(&agents, &self.id.to_string())?;
        let inbox = existing_directory(&agent, "inbox")?;
        for (actual, retained) in [
            (&agents, &self.agents),
            (&agent, &self.agent),
            (&inbox, &self.inbox),
            (&existing_directory(&inbox, ".done")?, &self.done),
            (&existing_directory(&inbox, ".staging")?, &self.staging),
            (&existing_directory(&agent, "outbox")?, &self.outbox),
        ] {
            if !same(&actual.metadata()?, &retained.metadata()?) {
                return Err(io::Error::other("mailbox directory identity changed"));
            }
        }
        Ok(())
    }
}

/// Reads every raw entry of `directory`, refusing once it has read `cap` and finds one more.
fn scan(directory: &Directory, cap: usize, mut entry: impl FnMut(&[u8])) -> io::Result<()> {
    let mut listing = Dir::read_from(&directory.file)?;
    let mut read = 0;
    while let Some(found) = listing.read() {
        let found = found?;
        let bytes = found.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if read == cap {
            return Err(io::Error::other(Defer::ScanOverflow(
                directory.label.clone(),
            )));
        }
        read += 1;
        directory
            .budget
            .entries
            .set(directory.budget.entries.get() + 1);
        entry(bytes);
    }
    Ok(())
}

fn snapshot(directory: &Directory, cap: usize) -> io::Result<Option<Vec<OsString>>> {
    let mut names = Vec::new();
    match scan(directory, cap, |bytes| {
        names.push(OsStr::from_bytes(bytes).to_owned());
    }) {
        Ok(()) => {
            names.sort();
            Ok(Some(names))
        }
        Err(error) if matches!(deferral(&error), Some(Defer::ScanOverflow(_))) => Ok(None),
        Err(error) => Err(error),
    }
}

fn named_canonical(directory: &Directory, name: &OsStr) -> io::Result<bool> {
    let (mut found, mut alias) = (false, false);
    scan(directory, directory.cap, |raw| {
        if raw.eq_ignore_ascii_case(name.as_bytes()) {
            if raw == name.as_bytes() {
                found = true;
            } else {
                alias = true;
            }
        }
    })?;
    if alias {
        return Err(io::Error::other("noncanonical mailbox filename"));
    }
    Ok(found)
}

fn snapshot_name(names: &[OsString], name: &OsStr) -> io::Result<bool> {
    if names
        .iter()
        .any(|entry| entry != name && entry.as_bytes().eq_ignore_ascii_case(name.as_bytes()))
    {
        return Err(io::Error::other("noncanonical mailbox filename"));
    }
    Ok(names.iter().any(|entry| entry == name))
}

fn marker_name(directory: &Directory, name: &OsStr) -> io::Result<()> {
    let names = snapshot(directory, INBOX_CAP)?
        .ok_or_else(|| io::Error::other(Defer::ScanOverflow(directory.label.clone())))?;
    if !names.iter().any(|entry| entry == name)
        || names
            .iter()
            .any(|entry| entry != name && entry.as_bytes().eq_ignore_ascii_case(name.as_bytes()))
    {
        return Err(io::Error::other("noncanonical acknowledgement filename"));
    }
    Ok(())
}

fn read_marker(directory: &Directory, name: &OsStr) -> io::Result<(File, Vec<u8>)> {
    marker_name(directory, name)?;
    read_file_inner(directory, name, MARKER_BYTES)
}

fn validate_marker(directory: &Directory, name: &OsStr, file: &File) -> io::Result<()> {
    marker_name(directory, name)?;
    validate_file_identity(directory, name, file)
}

fn deferred_marker_present(directory: &Directory, names: &[OsString], canonical: &str) -> bool {
    let folded = canonical.to_lowercase().to_uppercase();
    if names
        .iter()
        .any(|name| name.to_string_lossy().to_lowercase().to_uppercase() == folded)
    {
        return true;
    }
    !matches!(open(directory, canonical, OFlags::RDONLY), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn observed_done(directory: &Directory, names: &[OsString], canonical: &str) -> io::Result<bool> {
    let folded = canonical.to_lowercase().to_uppercase();
    if names.iter().any(|name| {
        name != OsStr::new(canonical)
            && name.to_string_lossy().to_lowercase().to_uppercase() == folded
    }) {
        return Err(io::Error::other("ambiguous acknowledgement filename"));
    }
    match open(directory, canonical, OFlags::RDONLY) {
        Ok(file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MARKER_BYTES {
                return Err(io::Error::other("ambiguous acknowledgement identity"));
            }
            validate_file_identity(directory, OsStr::new(canonical), &file)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Refuses the publishing rename when `.done` now holds the item's marker or any spelling of it.
fn unacknowledged(done: &Directory, name: &str) -> io::Result<()> {
    match snapshot(done, INBOX_CAP)? {
        Some(names) if !deferred_marker_present(done, &names, name) => Ok(()),
        _ => Err(io::Error::other(Defer::Acknowledged)),
    }
}

fn read_file(directory: &Directory, name: &OsStr, cap: u64) -> io::Result<(File, Vec<u8>)> {
    read_file_with(directory, name, cap, || Ok(()))
}

fn read_file_with(
    directory: &Directory,
    name: &OsStr,
    cap: u64,
    after_read: impl FnOnce() -> io::Result<()>,
) -> io::Result<(File, Vec<u8>)> {
    if !named_canonical(directory, name)? {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let (file, bytes) = read_file_inner(directory, name, cap)?;
    after_read()?;
    validate_file(directory, name, &file)?;
    Ok((file, bytes))
}

fn read_file_inner(directory: &Directory, name: &OsStr, cap: u64) -> io::Result<(File, Vec<u8>)> {
    let file = open(directory, name, OFlags::RDONLY)?;
    let before = file.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > cap {
        return Err(io::Error::other(
            "mailbox file identity or length is ambiguous",
        ));
    }
    let named = open(directory, name, OFlags::RDONLY)?;
    if !same(&before, &named.metadata()?) {
        return Err(io::Error::other("mailbox path changed"));
    }
    let mut bytes = Vec::new();
    (&file).take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(io::Error::other("mailbox decode bound exceeded"));
    }
    validate_file_identity(directory, name, &file)?;
    Ok((file, bytes))
}

fn validate_file(directory: &Directory, name: &OsStr, file: &File) -> io::Result<()> {
    if !named_canonical(directory, name)? {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    validate_file_identity(directory, name, file)
}

fn validate_file_identity(directory: &Directory, name: &OsStr, file: &File) -> io::Result<()> {
    let handle = file.metadata()?;
    let named = open(directory, name, OFlags::RDONLY)?.metadata()?;
    if !handle.is_file() || handle.nlink() != 1 || named.nlink() != 1 || !same(&handle, &named) {
        return Err(io::Error::other("mailbox file identity changed"));
    }
    Ok(())
}

fn canonical(name: &OsStr) -> Option<MailId> {
    let name = name.to_str()?;
    let id = name.strip_suffix(".json")?.parse::<MailId>().ok()?;
    (name == format!("{id}.json")).then_some(id)
}

fn remove(
    directory: &Directory,
    name: &OsStr,
    file: &File,
    validate: fn(&Directory, &OsStr, &File) -> io::Result<()>,
) -> io::Result<()> {
    validate(directory, name, file)?;
    rustix::fs::unlinkat(directory, name, AtFlags::empty())?;
    directory.sync()
}

fn verify_named_bytes(
    directory: &Directory,
    name: &str,
    file: &File,
    expected: &[u8],
) -> io::Result<()> {
    validate_file(directory, OsStr::new(name), file)?;
    let (named, bytes) = read_file(directory, OsStr::new(name), OUTBOX_BYTES)?;
    if !same(&file.metadata()?, &named.metadata()?) || bytes != expected {
        return Err(io::Error::other(
            "mailbox publication identity or bytes changed",
        ));
    }
    Ok(())
}

fn stage(tree: &Tree, name: &str, bytes: &[u8]) -> io::Result<File> {
    let file = match open(
        &tree.staging,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
    ) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let (file, found) = read_file(&tree.staging, OsStr::new(name), OUTBOX_BYTES)?;
            if found != bytes {
                return Err(io::Error::other("staging collision"));
            }
            file
        }
        Err(error) => return Err(error),
    };
    tree.budget.sync(&file)?;
    tree.staging.sync()?;
    tree.validate()?;
    verify_named_bytes(&tree.staging, name, &file, bytes)?;
    Ok(file)
}

fn publish(tree: &Tree, name: &str, bytes: &[u8]) -> io::Result<()> {
    publish_with(tree, name, bytes, || Ok(()))
}

fn publish_with(
    tree: &Tree,
    name: &str,
    bytes: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let raw = OsStr::new(name);
    if named_canonical(&tree.inbox, raw)? {
        let (file, found) = read_file(&tree.inbox, raw, OUTBOX_BYTES)?;
        if found != bytes {
            return Err(io::Error::other("delivery collision"));
        }
        tree.budget.sync(&file)?;
        tree.inbox.sync()?;
        tree.validate()?;
        return verify_named_bytes(&tree.inbox, name, &file, bytes);
    }
    let staged_name = format!("{name}.tmp");
    let file = stage(tree, &staged_name, bytes)?;
    if let Err(error) = before_rename() {
        let _ = remove(
            &tree.staging,
            OsStr::new(&staged_name),
            &file,
            validate_file,
        );
        return Err(error);
    }
    tree.validate()?;
    verify_named_bytes(&tree.staging, &staged_name, &file, bytes)?;
    rustix::fs::renameat_with(
        &tree.staging,
        staged_name.as_str(),
        &tree.inbox,
        name,
        RenameFlags::NOREPLACE,
    )?;
    tree.inbox.sync()?;
    tree.staging.sync()?;
    tree.validate()?;
    verify_named_bytes(&tree.inbox, name, &file, bytes)
}

struct StagedBody {
    rollback: bool,
    tree: Tree,
    file: File,
    name: String,
    body: String,
}

impl Drop for StagedBody {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        if let Err(error) = self.tree.validate().and_then(|()| {
            remove(
                &self.tree.staging,
                OsStr::new(&self.name),
                &self.file,
                validate_file,
            )
        }) {
            tracing::warn!(%error, file = %self.name, "unappended mail body cleanup failed");
        }
    }
}

impl StagedBody {
    fn submitted(&mut self) {
        self.rollback = false;
    }

    fn validate(&self) -> io::Result<()> {
        self.tree.validate()?;
        verify_named_bytes(
            &self.tree.staging,
            &self.name,
            &self.file,
            self.body.as_bytes(),
        )
    }
}

#[derive(Clone, Deserialize)]
struct Outgoing {
    #[serde(default)]
    to: Option<Address>,
    #[serde(default)]
    in_reply_to: Option<MailId>,
    act: MailAct,
    subject: String,
    #[serde(default)]
    body: Option<String>,
}

impl Outgoing {
    fn prepare(
        self,
        root: &Path,
        opening: Opening,
        sender: AgentId,
        recipient: AgentId,
        id: MailId,
        plan: &SendPlan,
    ) -> Result<(Item, Option<StagedBody>), MailboxProblem> {
        let mut item = Item {
            id,
            from: MailFrom::Agent { agent: sender },
            to: MailTo::Agent { agent: recipient },
            thread: Some(plan.thread),
            thread_seq: plan.sequence,
            members: plan.members.clone(),
            act: self.act,
            subject: self.subject,
            body_len: self.body.as_ref().map_or(0, |body| body.len() as u64),
            body: self.body,
            body_path: None,
            body_blake3: None,
        };
        let body_name = format!("{id}.body");
        let referenced = item.body.take_if(|body| body.len() > INLINE_BYTES);
        if let Some(body) = &referenced {
            item.body_path = Some(
                PathBuf::from("agents")
                    .join(recipient.to_string())
                    .join("inbox")
                    .join(&body_name),
            );
            item.body_blake3 = Some(dudes_journal::mail_body_digest(body.as_bytes()));
        }
        if serde_json::to_vec(&item).map_or(true, |bytes| bytes.len() as u64 > MARKER_BYTES) {
            return Err(MailboxProblem::OutboxCorruption);
        }
        let Some(body) = referenced else {
            return Ok((item, None));
        };
        let refused = |error: io::Error| match deferral(&error) {
            Some(Defer::ScanOverflow(_)) => MailboxProblem::ScanOverflow,
            _ => MailboxProblem::DeliveryCollision,
        };
        let tree = Tree::open(root, recipient, opening).map_err(refused)?;
        let staged = format!("{body_name}.tmp");
        let file = stage(&tree, &staged, body.as_bytes()).map_err(refused)?;
        Ok((
            item,
            Some(StagedBody {
                rollback: true,
                tree,
                file,
                name: staged,
                body,
            }),
        ))
    }
}

/// The recorded length and digest of an out-of-line body. `None` is no body or an inline one;
/// every other field combination is corruption.
fn referenced(item: &Item) -> io::Result<Option<(&Path, u64, &str)>> {
    match (
        &item.body,
        item.body_len,
        &item.body_path,
        &item.body_blake3,
    ) {
        (None, 0, None, None) => Ok(None),
        (Some(body), len, None, None) if len <= INLINE_BYTES as u64 && body.len() as u64 == len => {
            Ok(None)
        }
        (None, len, Some(path), Some(hash)) if len > INLINE_BYTES as u64 => {
            Ok(Some((path.as_path(), len, hash.as_str())))
        }
        _ => Err(io::Error::other("corrupt body fields")),
    }
}

impl Mailbox {
    pub(super) fn unix_unread_observation(&self, agent: AgentId) -> super::UnreadObservation {
        let selected: Vec<_> = self
            .pending
            .get(&agent)
            .into_iter()
            .flatten()
            .take(INBOX_CAP)
            .map(|(_, id)| &self.rows[id])
            .collect();
        if selected.is_empty() {
            return super::UnreadObservation::Complete(0);
        }
        let result = self.sample_markers(agent, &selected);
        if let Ok(count) = result {
            super::UnreadObservation::Complete(count)
        } else {
            self.diagnostic(agent, "inbox", MailboxProblem::AmbiguousMarker);
            *self
                .report
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                self.observation.borrow().clone();
            super::UnreadObservation::Incomplete
        }
    }

    fn sample_markers(&self, agent: AgentId, rows: &[&Row]) -> io::Result<u32> {
        self.sample_markers_with(agent, rows, |_| {})
    }

    fn sample_markers_with(
        &self,
        agent: AgentId,
        rows: &[&Row],
        mut before_read: impl FnMut(&str),
    ) -> io::Result<u32> {
        let budget = Rc::new(Budget::default());
        let tree = match Tree::open(&self.root, agent, self.opening(&budget, agent, false)) {
            Ok(tree) => tree,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && rows
                        .iter()
                        .all(|row| !self.published.contains(&row.item.id))
                    && std::fs::symlink_metadata(
                        self.root.join("agents").join(agent.to_string()),
                    )
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(0);
            }
            Err(error) => return Err(error),
        };
        tree.validate()?;
        let done_before = snapshot(&tree.done, INBOX_CAP)?
            .ok_or_else(|| io::Error::other("incomplete acknowledgement sample"))?;
        let inbox_before = snapshot(&tree.inbox, tree.inbox.cap)?
            .ok_or_else(|| io::Error::other("incomplete inbox sample"))?;
        let mut markers = Vec::new();
        for row in rows {
            if row.bounce {
                continue;
            }
            let name = format!("{}.json", row.item.id);
            if observed_done(&tree.done, &done_before, &name)? {
                continue;
            }
            before_read(&name);
            let marker = (|| {
                if !snapshot_name(&inbox_before, OsStr::new(&name))? {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                }
                let (file, bytes) = read_file_inner(&tree.inbox, OsStr::new(&name), MARKER_BYTES)?;
                let actual: Item = serde_json::from_slice(&bytes)?;
                if actual != row.item {
                    return Err(io::Error::other("inbox marker differs from journal"));
                }
                Ok(file)
            })();
            markers.push((name, row.item.id, marker));
        }
        let inbox_after = snapshot(&tree.inbox, tree.inbox.cap)?
            .ok_or_else(|| io::Error::other("incomplete inbox sample"))?;
        let markers: Vec<_> = markers
            .into_iter()
            .map(|(name, id, marker)| {
                let checked = marker.and_then(|file| {
                    if !snapshot_name(&inbox_after, OsStr::new(&name))? {
                        return Err(io::Error::from(io::ErrorKind::NotFound));
                    }
                    validate_file_identity(&tree.inbox, OsStr::new(&name), &file)
                });
                (name, id, checked)
            })
            .collect();
        tree.validate()?;
        let done_after = snapshot(&tree.done, INBOX_CAP)?
            .ok_or_else(|| io::Error::other("incomplete acknowledgement sample"))?;
        let mut count = 0;
        for (name, id, marker) in markers {
            if observed_done(&tree.done, &done_after, &name)? {
                continue;
            }
            match marker {
                Ok(()) => count += 1,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound && !self.published.contains(&id) => {
                }
                Err(error) => return Err(error),
            }
        }
        Ok(count)
    }

    /// Directory searches for `recipient` allow for every entry the journal accounts for: its
    /// recorded bodies and every agent directory.
    fn opening(&self, budget: &Rc<Budget>, recipient: AgentId, create: bool) -> Opening {
        Opening {
            budget: Rc::clone(budget),
            cap: SCAN_CAP
                + self.hired
                + self.bodies.len()
                + self.bodies.get(&recipient).copied().unwrap_or(0),
            create,
            turn: self.visits.get(&recipient).copied().unwrap_or(0),
        }
    }

    /// Records why an item, or the whole recipient, waits. Overflow defers the recipient.
    fn defer(
        &self,
        agent: AgentId,
        label: &str,
        error: &io::Error,
        otherwise: MailboxProblem,
    ) -> ControlFlow<()> {
        match deferral(error) {
            Some(Defer::Acknowledged) => ControlFlow::Continue(()),
            Some(Defer::ScanOverflow(directory)) => {
                self.diagnostic(agent, directory, MailboxProblem::ScanOverflow);
                ControlFlow::Break(())
            }
            None => {
                self.diagnostic(agent, label, otherwise);
                ControlFlow::Continue(())
            }
        }
    }

    pub(super) fn unix_pass(
        &mut self,
        agents: &BTreeMap<AgentId, bool>,
        now: &mut impl FnMut() -> Timestamp,
        mint: &mut impl FnMut() -> ThreadId,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
        mut before_observe: impl FnMut(AgentId),
        budget: &Rc<Budget>,
    ) {
        let mut append = |record: &Record, at: Option<Timestamp>| {
            budget.fsyncs.set(budget.fsyncs.get() + 1);
            append(record, at)
        };
        let recipients: BTreeSet<_> = agents.keys().chain(self.pending.keys()).copied().collect();
        self.observation
            .borrow_mut()
            .diagnostics
            .retain(|row| recipients.contains(&row.agent));
        self.visits.retain(|id, _| recipients.contains(id));
        let mut ordered: Vec<_> = recipients.iter().copied().collect();
        if let Some(next) = self.next {
            let start = ordered.partition_point(|id| *id < next);
            ordered.rotate_left(start);
        }
        for recipient in ordered.into_iter().take(RECIPIENTS_PER_PASS) {
            if budget.spent() {
                return;
            }
            self.observation
                .borrow_mut()
                .diagnostics
                .retain(|row| row.agent != recipient);
            before_observe(recipient);
            self.next = recipients
                .range((
                    std::ops::Bound::Excluded(recipient),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .copied()
                .or_else(|| recipients.first().copied());
            *self.visits.entry(recipient).or_default() += 1;
            let _ = self.visit(
                recipient,
                agents.get(&recipient),
                now,
                mint,
                &mut append,
                budget,
            );
            if self.fenced {
                return;
            }
        }
    }

    fn visit(
        &mut self,
        recipient: AgentId,
        agent: Option<&bool>,
        now: &mut impl FnMut() -> Timestamp,
        mint: &mut impl FnMut() -> ThreadId,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
        budget: &Rc<Budget>,
    ) -> ControlFlow<()> {
        let bounces: Vec<_> = self
            .pending
            .get(&recipient)
            .into_iter()
            .flatten()
            .map(|(_, id)| &self.rows[id])
            .filter(|row| row.bounce)
            .take(INBOX_CAP)
            .cloned()
            .collect();
        let mut started = false;
        for row in bounces {
            if !budget.item(&mut started) {
                break;
            }
            self.bounce(&row, append, budget)?;
        }
        let Some(agent) = agent else {
            return ControlFlow::Continue(());
        };
        let tree = match Tree::open(&self.root, recipient, self.opening(budget, recipient, true)) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = self.defer(recipient, ".done", &error, MailboxProblem::AmbiguousMarker);
                return ControlFlow::Continue(());
            }
        };
        if let Err(error) = self.remember_outbox(&tree) {
            let _ = self.defer(
                recipient,
                "outbox",
                &error,
                MailboxProblem::DeliveryCollision,
            );
        }
        self.observe(&tree, append)?;
        self.deliver(&tree)?;
        if !*agent {
            self.ingest(&tree, now, mint, append)?;
        }
        ControlFlow::Continue(())
    }

    fn observe(
        &mut self,
        tree: &Tree,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
    ) -> ControlFlow<()> {
        let mut names = match snapshot(&tree.done, INBOX_CAP) {
            Ok(Some(names)) if names.is_empty() => return ControlFlow::Continue(()),
            Ok(Some(names)) => match tree.validate() {
                Ok(()) => names,
                Err(error) => {
                    return self.defer(tree.id, ".done", &error, MailboxProblem::AmbiguousMarker);
                }
            },
            Ok(None) => {
                self.diagnostic(tree.id, ".done", MailboxProblem::ScanOverflow);
                return ControlFlow::Continue(());
            }
            Err(error) => {
                return self.defer(tree.id, ".done", &error, MailboxProblem::AmbiguousMarker);
            }
        };
        tree.order(&mut names);
        let mut started = false;
        for name in names {
            let label = name.to_string_lossy();
            let Some(id) = canonical(&name) else {
                self.diagnostic(tree.id, &label, MailboxProblem::AmbiguousMarker);
                continue;
            };
            let Some(row) = self.rows.get(&id).cloned() else {
                self.diagnostic(tree.id, &label, MailboxProblem::UnmatchedAcknowledgement);
                continue;
            };
            if row.item.to != (MailTo::Agent { agent: tree.id }) {
                self.diagnostic(tree.id, &label, MailboxProblem::RecipientMismatch);
                continue;
            }
            if row.bounce || matches!(row.state, MailState::Bounced { .. }) {
                self.diagnostic(tree.id, &label, MailboxProblem::UnmatchedAcknowledgement);
                continue;
            }
            if !tree.budget.item(&mut started) {
                break;
            }
            let (file, bytes) = match read_marker(&tree.done, &name) {
                Ok(read) => read,
                Err(error) => {
                    self.defer(tree.id, &label, &error, MailboxProblem::AmbiguousMarker)?;
                    continue;
                }
            };
            let Ok(marker) = serde_json::from_slice::<Item>(&bytes) else {
                self.diagnostic(tree.id, &label, MailboxProblem::MalformedMarker);
                continue;
            };
            if marker != row.item {
                self.diagnostic(tree.id, &label, MailboxProblem::MalformedMarker);
                continue;
            }
            let verified = tree
                .validate()
                .and_then(|()| validate_marker(&tree.done, &name, &file))
                .and_then(|()| Self::complete_body(tree, &row.item))
                .and_then(|()| tree.validate())
                .and_then(|()| validate_marker(&tree.done, &name, &file));
            if let Err(error) = verified {
                self.defer(tree.id, &label, &error, MailboxProblem::DeliveryCollision)?;
                continue;
            }
            if row.state != MailState::Acked
                && !self.persist(
                    &Record::MailState {
                        id,
                        state: MailState::Acked,
                    },
                    None,
                    append,
                )
            {
                return ControlFlow::Break(());
            }
            let _ = remove(&tree.done, &name, &file, validate_marker);
        }
        ControlFlow::Continue(())
    }

    fn complete_body(tree: &Tree, item: &Item) -> io::Result<()> {
        let Some((path, len, hash)) = referenced(item)? else {
            return Ok(());
        };
        let name = format!("{}.body", item.id);
        let expected = PathBuf::from("agents")
            .join(tree.id.to_string())
            .join("inbox")
            .join(&name);
        if path != expected {
            return Err(io::Error::other("noncanonical body reference"));
        }
        if named_canonical(&tree.inbox, OsStr::new(&name))? {
            let (file, body) = read_file(&tree.inbox, OsStr::new(&name), OUTBOX_BYTES)?;
            if body.len() as u64 != len || dudes_journal::mail_body_digest(&body) != hash {
                return Err(io::Error::other("body collision"));
            }
            tree.budget.sync(&file)?;
            tree.inbox.sync()?;
            return verify_named_bytes(&tree.inbox, &name, &file, &body);
        }
        let (_, body) = read_file(
            &tree.staging,
            OsStr::new(&format!("{name}.tmp")),
            OUTBOX_BYTES,
        )?;
        if body.len() as u64 != len || dudes_journal::mail_body_digest(&body) != hash {
            return Err(io::Error::other("staged body collision"));
        }
        publish(tree, &name, &body)
    }

    fn deliver(&mut self, tree: &Tree) -> ControlFlow<()> {
        self.deliver_with(tree, |_| {})
    }

    /// Publishes the earliest pending items not yet published in this run. The inbox is checked
    /// before `.done`, and `.done` again right before the rename, so an item the agent moves out
    /// of its inbox meanwhile is never published a second time.
    fn deliver_with(
        &mut self,
        tree: &Tree,
        mut before_publish: impl FnMut(&str),
    ) -> ControlFlow<()> {
        let Ok(Some(done)) = snapshot(&tree.done, INBOX_CAP) else {
            return ControlFlow::Continue(());
        };
        let mut slots: Vec<_> = self
            .pending
            .get(&tree.id)
            .into_iter()
            .flatten()
            .take(INBOX_CAP)
            .map(|(_, id)| *id)
            .collect();
        tree.order(&mut slots);
        let mut started = false;
        for id in slots {
            let row = &self.rows[&id];
            if row.bounce || self.published.contains(&id) {
                continue;
            }
            let item = row.item.clone();
            if !tree.budget.item(&mut started) {
                break;
            }
            let name = format!("{id}.json");
            if deferred_marker_present(&tree.done, &done, &name) {
                continue;
            }
            before_publish(&name);
            let outcome = Self::complete_body(tree, &item).and_then(|()| {
                let bytes = serde_json::to_vec(&item)?;
                publish_with(tree, &name, &bytes, || unacknowledged(&tree.done, &name))
            });
            match outcome {
                Ok(()) => {
                    self.published.insert(id);
                }
                Err(error) => {
                    self.defer(tree.id, &name, &error, MailboxProblem::DeliveryCollision)?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn bounce(
        &mut self,
        row: &Row,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
        budget: &Rc<Budget>,
    ) -> ControlFlow<()> {
        let MailTo::Agent { agent } = row.item.to else {
            return ControlFlow::Continue(());
        };
        let complete = match referenced(&row.item) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Tree::open(&self.root, agent, self.opening(budget, agent, false))
                .and_then(|tree| Self::complete_body(&tree, &row.item)),
            Err(error) => Err(error),
        };
        if let Err(error) = complete {
            return self.defer(
                agent,
                &format!("{}.body", row.item.id),
                &error,
                MailboxProblem::DeliveryCollision,
            );
        }
        if self.persist(
            &Record::MailState {
                id: row.item.id,
                state: MailState::Bounced {
                    why: "recipient is unknown or retired".to_owned(),
                },
            },
            None,
            append,
        ) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }

    fn prepare_fanout(
        &self,
        sender: &Tree,
        outbox: MailId,
        outgoing: &Outgoing,
        plan: &SendPlan,
    ) -> Result<Vec<(Item, Option<StagedBody>)>, MailboxProblem> {
        self.prepare_fanout_with(sender, outbox, outgoing, plan, |_, _| Ok(()))
    }

    fn prepare_fanout_with(
        &self,
        sender: &Tree,
        outbox: MailId,
        outgoing: &Outgoing,
        plan: &SendPlan,
        after_verify: impl FnOnce(&Tree, &Item) -> io::Result<()>,
    ) -> Result<Vec<(Item, Option<StagedBody>)>, MailboxProblem> {
        if let Some(receipt) = &plan.receipt {
            let same_body =
                match referenced(receipt).map_err(|_| MailboxProblem::DeliveryCollision)? {
                    None => receipt.body == outgoing.body,
                    Some((_, len, hash)) => {
                        let MailTo::Agent { agent } = receipt.to else {
                            return Err(MailboxProblem::DeliveryCollision);
                        };
                        let tree = Tree::open(
                            &self.root,
                            agent,
                            self.opening(&sender.budget, agent, false),
                        )
                        .map_err(|_| MailboxProblem::DeliveryCollision)?;
                        Self::complete_body(&tree, receipt)
                            .and_then(|()| after_verify(&tree, receipt))
                            .map_err(|_| MailboxProblem::DeliveryCollision)?;
                        outgoing.body.as_ref().is_some_and(|body| {
                            body.len() as u64 == len
                                && dudes_journal::mail_body_digest(body.as_bytes()) == hash
                        })
                    }
                };
            if receipt.act != outgoing.act || receipt.subject != outgoing.subject || !same_body {
                return Err(MailboxProblem::DeliveryCollision);
            }
        }
        let mut prepared = Vec::new();
        for recipient in &plan.recipients {
            let id = dudes_journal::mail_recipient_id(outbox, *recipient);
            if let Some(row) = self.rows.get(&id) {
                if row.item.thread != Some(plan.thread)
                    || row.item.thread_seq != plan.sequence
                    || row.item.members != plan.members
                    || row.item.from != (MailFrom::Agent { agent: sender.id })
                {
                    return Err(MailboxProblem::ReusedId);
                }
                continue;
            }
            prepared.push(outgoing.clone().prepare(
                &self.root,
                self.opening(&sender.budget, *recipient, true),
                sender.id,
                *recipient,
                id,
                plan,
            )?);
        }
        Ok(prepared)
    }

    fn ingest(
        &mut self,
        sender: &Tree,
        now: &mut impl FnMut() -> Timestamp,
        mint: &mut impl FnMut() -> ThreadId,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
    ) -> ControlFlow<()> {
        self.ingest_with(sender, now, mint, append, |_| Ok(()))
    }

    fn ingest_with(
        &mut self,
        sender: &Tree,
        now: &mut impl FnMut() -> Timestamp,
        mint: &mut impl FnMut() -> ThreadId,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
        mut before_append: impl FnMut(&OsStr) -> io::Result<()>,
    ) -> ControlFlow<()> {
        let Ok(Some(mut names)) = snapshot(&sender.outbox, OUTBOX_SCAN) else {
            self.diagnostic(sender.id, "outbox", MailboxProblem::ScanOverflow);
            return ControlFlow::Continue(());
        };
        sender.order(&mut names);
        let mut started = false;
        for name in names {
            let label = name.to_string_lossy();
            let Some(outbox) = canonical(&name) else {
                self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                continue;
            };
            if self.rows.contains_key(&outbox) {
                self.diagnostic(sender.id, &label, MailboxProblem::ReusedId);
                continue;
            }
            if !sender.budget.item(&mut started) {
                break;
            }
            let (source, bytes) = match read_file(&sender.outbox, &name, OUTBOX_BYTES) {
                Ok(read) => read,
                Err(error) => {
                    self.defer(sender.id, &label, &error, MailboxProblem::OutboxCorruption)?;
                    continue;
                }
            };
            let Ok(outgoing) = serde_json::from_slice::<Outgoing>(&bytes) else {
                self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                continue;
            };
            let Ok(route) = Route::parse(outgoing.to.as_ref(), outgoing.in_reply_to) else {
                self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                continue;
            };
            let plan = match self
                .mail_index
                .plan(sender.id, outbox, route, &self.rows, mint)
            {
                Ok(plan) => plan,
                Err(reason) => {
                    tracing::debug!(agent = %sender.id, %reason, "mail admission refused");
                    self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                    continue;
                }
            };
            let prepared = match self.prepare_fanout(sender, outbox, &outgoing, &plan) {
                Ok(prepared) => prepared,
                Err(problem) => {
                    self.diagnostic(sender.id, &label, problem);
                    continue;
                }
            };
            if before_append(&name).is_err()
                || sender
                    .validate()
                    .and_then(|()| verify_named_bytes(&sender.outbox, &label, &source, &bytes))
                    .is_err()
            {
                self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                continue;
            }
            if prepared
                .iter()
                .any(|(_, body)| body.as_ref().is_some_and(|body| body.validate().is_err()))
            {
                self.diagnostic(sender.id, &label, MailboxProblem::DeliveryCollision);
                continue;
            }
            let mut admitted_at = if plan.receipt.is_none() {
                let at = now();
                if self.mail_index.admit_day(sender.id, at).is_err() {
                    self.diagnostic(sender.id, &label, MailboxProblem::OutboxCorruption);
                    continue;
                }
                Some(at)
            } else {
                None
            };
            for (item, mut staged_body) in prepared {
                let id = item.id;
                let record = Record::from(item);
                if let Some(body) = &mut staged_body {
                    body.submitted();
                }
                if !self.persist(&record, admitted_at.take(), append) {
                    return ControlFlow::Break(());
                }
                let row = self.rows[&id].clone();
                if row.bounce {
                    let _ = self.bounce(&row, append, &sender.budget);
                    if self.fenced {
                        return ControlFlow::Break(());
                    }
                }
            }
            let _ = remove(&sender.outbox, &name, &source, validate_file);
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{DIAGNOSTICS_CAP, LABEL_BYTES};
    use dudes_journal::{Fsync, JournalOptions, Reader, Writer};

    use dudes_supervisor::World;
    use dudes_types::Timestamp;
    use serde_json::json;

    fn mint_thread() -> ThreadId {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        ThreadId::from_parts(
            1,
            u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
        )
    }

    #[test]
    fn immediate_known_codex_hire_prepares_only_its_owned_mailbox() {
        let h = Harness::new(2);
        let agent = Harness::id(1);
        std::fs::remove_dir_all(h.root.path().join("agents").join(agent.to_string())).unwrap();
        let mut mailbox = h.mailbox;
        mailbox.codex_agents.insert(agent);
        let grant = mailbox.admit_outbox(agent).unwrap();
        assert_eq!(
            grant.path(),
            h.root
                .path()
                .canonicalize()
                .unwrap()
                .join("agents")
                .join(agent.to_string())
                .join("outbox")
        );
        grant.check().unwrap();
        assert!(mailbox.admit_outbox(Harness::id(9)).is_err());
        assert!(
            !h.root
                .path()
                .join("agents")
                .join(Harness::id(9).to_string())
                .exists()
        );
    }

    #[test]
    fn outbox_identity_survives_periodic_setup_and_refuses_missing_replaced_or_aliased_roots() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        for change in ["missing", "replaced", "inbox", "other"] {
            let mut h = Harness::new(2);
            let agent = Harness::id(1);
            h.mailbox.codex_agents.insert(agent);
            let grant = h.mailbox.admit_outbox(agent).unwrap();
            let path = grant.path().to_owned();
            std::fs::rename(&path, h.root.path().join("retained-outbox")).unwrap();
            match change {
                "missing" => {}
                "replaced" => {
                    std::fs::create_dir(&path).unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                "inbox" => symlink(
                    h.root
                        .path()
                        .join("agents")
                        .join(agent.to_string())
                        .join("inbox"),
                    &path,
                )
                .unwrap(),
                _ => symlink(
                    h.root
                        .path()
                        .join("agents")
                        .join(Harness::id(2).to_string())
                        .join("outbox"),
                    &path,
                )
                .unwrap(),
            }
            assert!(grant.check().is_err(), "changed root {change}");
            assert!(
                h.mailbox.admit_outbox(agent).is_err(),
                "resumed grant {change}"
            );
            h.pass();
            assert!(
                h.mailbox.admit_outbox(agent).is_err(),
                "periodic setup must retain identity for {change}"
            );
        }
    }

    #[test]
    fn an_ancestor_alias_cannot_retarget_the_retained_tree_away_from_its_granted_path() {
        use std::os::unix::fs::{DirBuilderExt as _, symlink};

        let mut h = Harness::new(2);
        let owner = tempfile::tempdir().unwrap();
        let original_parent = owner.path().join("original-parent");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&original_parent)
            .unwrap();
        let alias = owner.path().join("state-alias");
        let original = original_parent.join("state");
        std::fs::rename(h.root.path(), &original).unwrap();
        symlink(&original_parent, &alias).unwrap();
        h.mailbox.root = alias.join("state");
        let agent = Harness::id(1);
        h.mailbox.codex_agents.insert(agent);
        let grant = h.mailbox.admit_outbox(agent).unwrap();
        grant.check().unwrap();

        let moved = owner.path().join("moved-parent");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&moved)
            .unwrap();
        std::fs::rename(&original, moved.join("state")).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&moved, &alias).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&original)
            .unwrap();
        let replacement = Tree::open(&original, agent, Opening::default()).unwrap();
        assert!(replacement.outbox_grant().is_ok());
        assert!(grant.path().is_dir());
        assert!(
            grant.check().is_err(),
            "the grant's frozen canonical path now names a replacement tree"
        );
        assert!(h.mailbox.admit_outbox(agent).is_err());
    }

    #[test]
    fn outbox_grant_refuses_agent_aliases_and_nonprivate_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let mut h = Harness::new(2);
        let agent = Harness::id(1);
        h.mailbox.codex_agents.insert(agent);
        let outbox = h
            .root
            .path()
            .join("agents")
            .join(agent.to_string())
            .join("outbox");
        std::fs::set_permissions(&outbox, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(h.mailbox.admit_outbox(agent).is_err());
        std::fs::set_permissions(&outbox, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = outbox.parent().unwrap();
        std::fs::rename(directory, h.root.path().join("retained-agent")).unwrap();
        symlink(
            h.root
                .path()
                .join("agents")
                .join(Harness::id(2).to_string()),
            directory,
        )
        .unwrap();
        assert!(h.mailbox.admit_outbox(agent).is_err());
    }

    struct Harness {
        root: tempfile::TempDir,
        mailbox: Mailbox,
        writer: Writer,
        agents: BTreeMap<AgentId, bool>,
        records: Vec<Record>,
        at: Timestamp,
    }

    impl Harness {
        fn new(count: usize) -> Self {
            let root = tempfile::tempdir().unwrap();
            let journal = root.path().join("journal");
            let (mut writer, _) = dudes_journal::open(
                &journal,
                JournalOptions {
                    fsync: Fsync::Full,
                    ..JournalOptions::default()
                },
            )
            .unwrap();
            let mut mailbox =
                Mailbox::replay(root.path().to_path_buf(), &Reader::new(&journal)).unwrap();
            let mut world = World::default();
            for n in 1..=count {
                let id = AgentId::from_parts(1, n as u128);
                let record: Record = serde_json::from_value(json!({"t":"agent_hired", "id":id, "name":"agent", "role":"worker", "department":"test", "provider":"claude", "cwd":root.path(), "budget":{"usd_per_day":null,"tokens_per_day":null}})).unwrap();
                let entry = Entry {
                    at: Timestamp::from_unix_millis(1),
                    record,
                };
                let lsn = writer.append(&entry).unwrap();
                world.apply(lsn, &entry);
                mailbox.apply(lsn, &entry.record);
                Tree::open(root.path(), id, Opening::default()).unwrap();
            }
            Self {
                root,
                mailbox,
                writer,
                agents: world
                    .agents
                    .into_iter()
                    .map(|(id, row)| (id, row.retired))
                    .collect(),
                records: Vec::new(),
                at: Timestamp::from_unix_millis(1),
            }
        }

        fn id(n: u128) -> AgentId {
            AgentId::from_parts(1, n)
        }
        fn mail(n: u128) -> MailId {
            MailId::from_parts(2, n)
        }
        fn path(&self, agent: AgentId, directory: &str, name: &str) -> PathBuf {
            self.root
                .path()
                .join("agents")
                .join(agent.to_string())
                .join(directory)
                .join(name)
        }
        fn retire(&mut self, agent: AgentId) {
            let record = Record::AgentRetired {
                id: agent,
                reason: "test retirement".to_owned(),
                browser_retire: None,
            };
            let lsn = self
                .writer
                .append(&Entry {
                    at: Timestamp::from_unix_millis(1),
                    record: record.clone(),
                })
                .unwrap();
            self.mailbox.apply(lsn, &record);
            *self.agents.get_mut(&agent).unwrap() = true;
        }
        fn send(&self, sender: AgentId, recipient: AgentId, id: MailId, body: &str) {
            std::fs::write(self.path(sender, "outbox", &format!("{id}.json")), serde_json::to_vec(&json!({"to":{"kind":"agent","agent":recipient},"act":"message","subject":"subject", "body":body, "from":{"kind":"human"}, "actor":{"kind":"system"}})).unwrap()).unwrap();
        }
        fn outgoing(&self, sender: AgentId, outbox: MailId, value: &serde_json::Value) {
            std::fs::write(
                self.path(sender, "outbox", &format!("{outbox}.json")),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
        }
        fn pass_at(&mut self, at: Timestamp) {
            let writer = &mut self.writer;
            let records = &mut self.records;
            self.mailbox.pass(
                &self.agents,
                || at,
                mint_thread,
                |record, admitted_at| {
                    let entry = Entry {
                        at: admitted_at.unwrap_or(at),
                        record: record.clone(),
                    };
                    let lsn = writer.append(&entry)?;
                    records.push(record.clone());
                    Ok((lsn, entry))
                },
            );
        }
        fn pass(&mut self) {
            let at = self.at;
            let writer = &mut self.writer;
            let records = &mut self.records;
            self.mailbox.pass(
                &self.agents,
                || at,
                mint_thread,
                |record, admitted_at| {
                    let lsn = writer.append(&Entry {
                        at: admitted_at.unwrap_or(at),
                        record: record.clone(),
                    })?;
                    records.push(record.clone());
                    Ok((
                        lsn,
                        Entry {
                            at: admitted_at.unwrap_or(at),
                            record: record.clone(),
                        },
                    ))
                },
            );
        }
        fn pass_within(&mut self, budget: &Rc<Budget>) {
            let at = self.at;
            let writer = &mut self.writer;
            let records = &mut self.records;
            self.mailbox.unix_pass(
                &self.agents,
                &mut || at,
                &mut mint_thread,
                &mut |record, admitted_at| {
                    let lsn = writer.append(&Entry {
                        at: admitted_at.unwrap_or(at),
                        record: record.clone(),
                    })?;
                    records.push(record.clone());
                    Ok((
                        lsn,
                        Entry {
                            at: admitted_at.unwrap_or(at),
                            record: record.clone(),
                        },
                    ))
                },
                |_| {},
                budget,
            );
        }
        fn settle(&mut self) {
            for _ in 0..16 {
                self.pass();
            }
        }
        /// Recorded, acknowledged out-of-line mail to `recipient`, with each body in its inbox.
        fn record_history(&mut self, recipient: AgentId, count: u64) {
            for n in 0..count {
                let id = Harness::mail(10_000 + u128::from(n));
                let name = format!("{id}.body");
                std::fs::write(self.path(recipient, "inbox", &name), "").unwrap();
                let record = Record::Mail {
                    thread: None,
                    thread_seq: 0,
                    members: Vec::new(),
                    id,
                    from: MailFrom::Agent {
                        agent: Harness::id(1),
                    },
                    to: MailTo::Agent { agent: recipient },
                    act: MailAct::Message,
                    subject: "history".to_owned(),
                    body: None,
                    body_len: INLINE_BYTES as u64 + 1,
                    body_path: Some(
                        PathBuf::from("agents")
                            .join(recipient.to_string())
                            .join("inbox")
                            .join(name),
                    ),
                    body_blake3: Some(dudes_journal::mail_body_digest(b"history")),
                    state: MailState::Acked,
                    actor: None,
                    provenance: None,
                };
                self.mailbox.apply(Lsn(1_000_000 + n), &record);
            }
            self.at = Timestamp::from_unix_millis(86_400_000);
        }
        fn diagnosed(&self, agent: AgentId, filename: &str, problem: MailboxProblem) -> bool {
            self.mailbox
                .report
                .lock()
                .unwrap()
                .diagnostics
                .iter()
                .any(|row| row.agent == agent && row.filename == filename && row.problem == problem)
        }
        fn move_done(&self, recipient: AgentId, id: MailId) -> PathBuf {
            let done = self.path(recipient, "inbox/.done", &format!("{id}.json"));
            std::fs::rename(self.path(recipient, "inbox", &format!("{id}.json")), &done).unwrap();
            done
        }
        fn acked(&self, id: MailId) -> usize {
            self.records.iter().filter(|record| matches!(record, Record::MailState { id: found, state: MailState::Acked } if *found == id)).count()
        }
        fn json_count(&self, recipient: AgentId) -> usize {
            std::fs::read_dir(self.path(recipient, "inbox", ""))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count()
        }
    }

    #[test]
    fn unread_observation_counts_only_published_earliest_markers_and_never_writes() {
        let mut h = Harness::new(3);
        let recipient = Harness::id(1);
        for n in 1..=20 {
            h.send(Harness::id(2), recipient, Harness::mail(n), "inline");
            h.send(Harness::id(3), recipient, Harness::mail(100 + n), "inline");
        }
        h.pass();
        assert!(h.mailbox.pending[&recipient].len() <= 32);
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Complete(0)
        );
        h.settle();
        assert_eq!(h.mailbox.pending[&recipient].len(), 40);
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Complete(32)
        );
        let records = h.records.clone();
        let id = h.mailbox.pending[&recipient].first().unwrap().1;
        h.move_done(recipient, id);
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Complete(31)
        );
        assert_eq!(h.records, records);
        assert_eq!(h.acked(id), 0);
        let second = h.mailbox.pending[&recipient].iter().nth(1).unwrap().1;
        std::fs::write(
            h.path(recipient, "inbox", &format!("{second}.json")),
            "corrupt",
        )
        .unwrap();
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Incomplete
        );
        assert!(h.diagnosed(recipient, "inbox", MailboxProblem::AmbiguousMarker));
        assert_eq!(h.records, records);
    }

    #[test]
    fn unread_observation_counts_markers_without_reading_or_repairing_bodies() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        h.send(
            Harness::id(1),
            recipient,
            Harness::mail(1),
            &"body".repeat(2_000),
        );
        h.settle();
        let id = h.mailbox.pending[&recipient].first().unwrap().1;
        let body = h
            .root
            .path()
            .join(h.mailbox.rows[&id].item.body_path.as_ref().unwrap());
        let marker = h.path(recipient, "inbox", &format!("{id}.json"));
        let bytes = std::fs::read(&marker).unwrap();
        let records = h.records.clone();
        std::fs::remove_file(&body).unwrap();
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Complete(1)
        );
        assert!(!body.exists());
        assert_eq!(std::fs::read(marker).unwrap(), bytes);
        assert_eq!(h.records, records);
    }

    #[test]
    fn unread_observation_accepts_legacy_markers_and_refuses_known_missing_publication() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        h.send(Harness::id(1), recipient, Harness::mail(1), "inline");
        h.settle();
        let id = h.mailbox.pending[&recipient].first().unwrap().1;
        let marker = h.path(recipient, "inbox", &format!("{id}.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        let row = h.mailbox.rows.get_mut(&id).unwrap();
        row.item.thread = None;
        row.item.thread_seq = 0;
        row.item.members.clear();
        for field in ["thread", "thread_seq", "members"] {
            value.as_object_mut().unwrap().remove(field);
        }
        std::fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Complete(1)
        );
        std::fs::remove_file(marker).unwrap();
        assert_eq!(
            Mailbox::unread_observation(Some(&h.mailbox), recipient),
            super::super::UnreadObservation::Incomplete
        );
    }

    #[test]
    fn unread_observation_measures_retained_history_and_multiple_agents() {
        let mut h = Harness::new(8);
        let recipient = Harness::id(2);
        h.record_history(recipient, 1400);
        for n in 1..=32 {
            h.send(Harness::id(1), recipient, Harness::mail(n), "inline");
        }
        h.settle();
        assert_eq!(h.mailbox.bodies[&recipient], 1400);
        assert_eq!(h.mailbox.pending[&recipient].len(), 32);
        let records = h.records.clone();
        let start = std::time::Instant::now();
        for _ in 0..20 {
            for n in 1..=8 {
                assert_eq!(
                    Mailbox::unread_observation(Some(&h.mailbox), Harness::id(n)),
                    super::super::UnreadObservation::Complete(if n == 2 { 32 } else { 0 })
                );
            }
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "20 samples with 1400 retained bodies and 32 markers took {elapsed:?}"
        );
        assert_eq!(h.records, records);
    }

    #[test]
    fn unread_observation_rechecks_done_when_a_marker_moves_during_the_sample() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        h.send(Harness::id(1), recipient, Harness::mail(1), "inline");
        h.settle();
        let rows: Vec<_> = h.mailbox.pending[&recipient]
            .iter()
            .map(|(_, id)| &h.mailbox.rows[id])
            .collect();
        let records = h.records.clone();
        let result = h.mailbox.sample_markers_with(recipient, &rows, |name| {
            std::fs::rename(
                h.path(recipient, "inbox", name),
                h.path(recipient, "inbox/.done", name),
            )
            .unwrap();
        });
        assert_eq!(result.unwrap(), 0);
        assert_eq!(h.records, records);
    }

    #[test]
    fn threaded_group_send_uses_structural_sender_and_fixed_membership() {
        let mut h = Harness::new(3);
        let id = Harness::mail(1);
        let sender = Harness::id(1);
        std::fs::write(
            h.path(sender, "outbox", &format!("{id}.json")),
            serde_json::to_vec(&json!({"to":{"agents":[Harness::id(2), Harness::id(3)]},
                "act":"message", "subject":"group", "body":"hello",
                "from":{"kind":"agent","agent":Harness::id(3)}}))
            .unwrap(),
        )
        .unwrap();
        h.pass();
        let rows: Vec<_> = h
            .records
            .iter()
            .filter_map(|record| {
                if let Record::Mail {
                    from,
                    to,
                    thread,
                    thread_seq,
                    members,
                    ..
                } = record
                {
                    Some((*from, *to, *thread, *thread_seq, members.clone()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, MailFrom::Agent { agent: sender });
        assert_eq!(
            rows[0].1,
            MailTo::Agent {
                agent: Harness::id(2)
            }
        );
        assert_eq!(
            rows[1].1,
            MailTo::Agent {
                agent: Harness::id(3)
            }
        );
        assert!(rows[0].2.is_some());
        assert_eq!(rows[0].2, rows[1].2);
        assert_eq!(rows[0].3, 1);
        assert_eq!(rows[0].4, vec![sender, Harness::id(2), Harness::id(3)]);
    }

    #[test]
    fn threaded_explicit_input_cap_precedes_sender_exclusion() {
        let mut h = Harness::new(8);
        h.outgoing(
            Harness::id(1),
            Harness::mail(1),
            &json!({"to":{"agents":
            (1..=8).map(Harness::id).collect::<Vec<_>>()}, "act":"message","subject":"too many"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 0);
        h.outgoing(
            Harness::id(1),
            Harness::mail(2),
            &json!({"to":{"agents":
            (2..=8).map(Harness::id).collect::<Vec<_>>()}, "act":"message","subject":"seven"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 7);
        for row in h.mailbox.rows.values() {
            assert_eq!(row.item.members.len(), 8);
        }
    }

    #[test]
    fn threaded_reply_cap_counts_journal_max_even_when_citing_opener() {
        let mut h = Harness::new(2);
        let outbox = Harness::mail(1);
        h.outgoing(
            Harness::id(1),
            outbox,
            &json!({"to":{"agent":Harness::id(2)},
            "act":"message","subject":"open"}),
        );
        h.pass();
        let parent = dudes_journal::mail_recipient_id(outbox, Harness::id(2));
        for n in 2..=12 {
            h.outgoing(
                Harness::id(2),
                Harness::mail(n),
                &json!({"in_reply_to":parent,
                "act":"message","subject":"reply"}),
            );
            h.pass();
            assert_eq!(h.mailbox.rows.len(), n as usize);
        }
        h.outgoing(
            Harness::id(2),
            Harness::mail(13),
            &json!({"in_reply_to":parent,
            "act":"message","subject":"refused"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 12);
        assert_eq!(
            h.mailbox.rows.values().map(|row| row.item.thread_seq).max(),
            Some(12)
        );
    }

    #[test]
    fn threaded_daily_allowance_counts_logical_sends_and_rolls_at_utc_midnight() {
        let mut h = Harness::new(3);
        for n in 1..=32 {
            h.outgoing(
                Harness::id(1),
                Harness::mail(n),
                &json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},
                "act":"message","subject":"group"}),
            );
            h.pass_at(Timestamp::from_unix_millis(86_399_999));
        }
        assert_eq!(h.mailbox.rows.len(), 64);
        h.outgoing(
            Harness::id(1),
            Harness::mail(33),
            &json!({"to":{"agent":Harness::id(2)},
            "act":"message","subject":"next"}),
        );
        h.pass_at(Timestamp::from_unix_millis(86_399_999));
        assert_eq!(h.mailbox.rows.len(), 64);
        h.mailbox = Mailbox::replay(
            h.root.path().to_owned(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.pass_at(Timestamp::from_unix_millis(86_400_000));
        assert_eq!(h.mailbox.rows.len(), 65);
    }

    #[test]
    fn threaded_every_durable_prefix_recovers_without_duplicate_receipts() {
        for prefix in 0..=3 {
            let mut h = Harness::new(4);
            let sender = Harness::id(1);
            let outbox = Harness::mail(1);
            h.outgoing(
                sender,
                outbox,
                &json!({"to":{"everyone":true},"act":"request",
                "subject":"group","body":"same"}),
            );
            let writer = &mut h.writer;
            let mut appended = 0;
            h.mailbox.pass(
                &h.agents,
                || Timestamp::from_unix_millis(1),
                mint_thread,
                |record, admitted_at| {
                    if appended == prefix {
                        anyhow::bail!("crash after durable prefix");
                    }
                    let entry = Entry {
                        at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                        record: record.clone(),
                    };
                    let lsn = writer.append(&entry)?;
                    appended += 1;
                    Ok((lsn, entry))
                },
            );
            h.mailbox = Mailbox::replay(
                h.root.path().to_owned(),
                &Reader::new(&h.root.path().join("journal")),
            )
            .unwrap();
            if prefix > 0 {
                h.retire(Harness::id(4));
                let parent = dudes_journal::mail_recipient_id(outbox, Harness::id(2));
                h.outgoing(
                    Harness::id(2),
                    Harness::mail(2),
                    &json!({"in_reply_to":parent,
                    "act":"message","subject":"advances"}),
                );
                let tree = Tree::open(h.root.path(), Harness::id(2), Opening::default()).unwrap();
                let writer = &mut h.writer;
                let at = Timestamp::from_unix_millis(86_400_001);
                let _ = h.mailbox.ingest(
                    &tree,
                    &mut || at,
                    &mut mint_thread,
                    &mut |record, admitted_at| {
                        let entry = Entry {
                            at: admitted_at.unwrap_or(at),
                            record: record.clone(),
                        };
                        let lsn = writer.append(&entry)?;
                        Ok((lsn, entry))
                    },
                );
            }
            h.pass_at(Timestamp::from_unix_millis(86_400_001));
            let reader = Reader::new(&h.root.path().join("journal"));
            let receipts: Vec<_> = reader
                .iter_from(Lsn::ZERO)
                .unwrap()
                .filter_map(|entry| {
                    let (_, entry) = entry.unwrap();
                    if let Record::Mail {
                        subject,
                        id,
                        thread_seq,
                        members,
                        ..
                    } = entry.record
                    {
                        (subject == "group").then_some((id, thread_seq, members))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(receipts.len(), 3, "prefix {prefix}");
            assert_eq!(
                receipts
                    .iter()
                    .map(|row| row.0)
                    .collect::<BTreeSet<_>>()
                    .len(),
                3
            );
            for receipt in receipts {
                assert_eq!(receipt.1, 1);
                assert_eq!(receipt.2, (1..=4).map(Harness::id).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn threaded_partial_recovery_refuses_changed_content_and_corrupt_staged_body() {
        for change in ["subject", "act", "body", "route", "corrupt", "missing"] {
            let mut h = Harness::new(3);
            let outbox = Harness::mail(1);
            let sender = Harness::id(1);
            let mut value = json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},
                "act":"message","subject":"group","body":"x".repeat(5000)});
            h.outgoing(sender, outbox, &value);
            let writer = &mut h.writer;
            let mut appended = 0;
            h.mailbox.pass(
                &h.agents,
                || Timestamp::from_unix_millis(1),
                mint_thread,
                |record, admitted_at| {
                    if appended == 1 {
                        anyhow::bail!("crash");
                    }
                    let entry = Entry {
                        at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                        record: record.clone(),
                    };
                    let lsn = writer.append(&entry)?;
                    appended += 1;
                    Ok((lsn, entry))
                },
            );
            let id = dudes_journal::mail_recipient_id(outbox, Harness::id(2));
            let staged = h.path(Harness::id(2), "inbox/.staging", &format!("{id}.body.tmp"));
            match change {
                "subject" => value["subject"] = json!("changed"),
                "act" => value["act"] = json!("request"),
                "body" => value["body"] = json!("changed"),
                "route" => value["to"] = json!({"agents":[Harness::id(2)]}),
                "corrupt" => std::fs::write(&staged, "corrupt").unwrap(),
                "missing" => std::fs::remove_file(&staged).unwrap(),
                _ => unreachable!(),
            }
            h.outgoing(sender, outbox, &value);
            h.mailbox = Mailbox::replay(
                h.root.path().to_owned(),
                &Reader::new(&h.root.path().join("journal")),
            )
            .unwrap();
            h.pass();
            assert_eq!(h.mailbox.rows.len(), 1, "{change}");
        }
    }

    #[test]
    fn threaded_reply_preserves_members_and_refuses_outsiders() {
        let mut h = Harness::new(4);
        let outbox = Harness::mail(1);
        h.outgoing(
            Harness::id(1),
            outbox,
            &json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},
            "act":"message","subject":"open"}),
        );
        h.pass();
        let parent = dudes_journal::mail_recipient_id(outbox, Harness::id(2));
        h.outgoing(
            Harness::id(4),
            Harness::mail(2),
            &json!({"in_reply_to":parent,"act":"message","subject":"outsider"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 2);
        h.outgoing(
            Harness::id(2),
            Harness::mail(3),
            &json!({"in_reply_to":parent,"act":"message","subject":"member"}),
        );
        h.pass();
        let recipients: Vec<_> = h
            .mailbox
            .rows
            .values()
            .filter(|row| row.item.subject == "member")
            .map(|row| row.item.to)
            .collect();
        assert_eq!(recipients.len(), 2);
        assert!(recipients.contains(&MailTo::Agent {
            agent: Harness::id(1)
        }));
        assert!(recipients.contains(&MailTo::Agent {
            agent: Harness::id(3)
        }));
    }

    #[test]
    fn threaded_department_uses_trimmed_case_sensitive_live_roster() {
        let mut h = Harness::new(4);
        h.retire(Harness::id(4));
        h.outgoing(
            Harness::id(1),
            Harness::mail(1),
            &json!({"to":{"department":"Test"},"act":"message","subject":"wrong case"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 0);
        h.outgoing(
            Harness::id(1),
            Harness::mail(2),
            &json!({"to":{"department":" \ttest\r "},"act":"message","subject":"trimmed"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 2);
        for row in h.mailbox.rows.values() {
            assert_eq!(
                row.item.members,
                vec![Harness::id(1), Harness::id(2), Harness::id(3)]
            );
        }
    }

    #[test]
    fn threaded_ordinary_entry_fold_uses_canonical_day_and_replay_agrees() {
        let mut h = Harness::new(2);
        let at = Timestamp::from_unix_millis(172_799_999);
        for n in 1..=32 {
            let record: Record = serde_json::from_value(json!({"t":"mail", "id":Harness::mail(n),
                "from":{"kind":"agent","agent":Harness::id(1)}, "to":{"kind":"agent","agent":Harness::id(2)},
                "act":"message", "subject":"legacy", "body_len":0, "state":{"state":"durable"}})).unwrap();
            let entry = Entry { at, record };
            let lsn = h.writer.append(&entry).unwrap();
            h.mailbox.apply_entry(lsn, &entry);
        }
        let outbox = Harness::mail(33);
        h.outgoing(
            Harness::id(1),
            outbox,
            &json!({"to":{"agent":Harness::id(2)},"act":"message","subject":"fresh"}),
        );
        h.pass_at(at);
        assert_eq!(h.mailbox.rows.len(), 32);
        h.mailbox = Mailbox::replay(
            h.root.path().to_owned(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.pass_at(at);
        assert_eq!(h.mailbox.rows.len(), 32);
        h.pass_at(Timestamp::from_unix_millis(172_800_000));
        assert_eq!(h.mailbox.rows.len(), 33);
        let entry = Reader::new(&h.root.path().join("journal"))
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .map(|(_, entry)| entry)
            .last()
            .unwrap();
        assert_eq!(entry.at, Timestamp::from_unix_millis(172_800_000));
    }

    #[test]
    fn threaded_collision_and_invalid_routing_refuse_before_any_receipt_append() {
        let mut h = Harness::new(3);
        let sender = Harness::id(1);
        let outbox = Harness::mail(1);
        let collision = dudes_journal::mail_recipient_id(outbox, Harness::id(2));
        let entry = Entry { at: Timestamp::from_unix_millis(1), record:
            serde_json::from_value(json!({"t":"mail", "id":collision,
                "from":{"kind":"agent","agent":Harness::id(3)}, "to":{"kind":"agent","agent":Harness::id(2)},
                "act":"message", "subject":"original", "body_len":0, "state":{"state":"durable"}})).unwrap() };
        let lsn = h.writer.append(&entry).unwrap();
        h.mailbox.apply_entry(lsn, &entry);
        h.outgoing(sender, outbox, &json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},"act":"message","subject":"collision"}));
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 1);
        h.outgoing(sender, Harness::mail(2), &json!({"to":{"agent":Harness::id(2)},"in_reply_to":collision,"act":"message","subject":"both routes"}));
        h.outgoing(
            sender,
            Harness::mail(3),
            &json!({"act":"message","subject":"no route"}),
        );
        h.outgoing(
            sender,
            Harness::mail(4),
            &json!({"to":{"agent":sender},"act":"message","subject":"self"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 1);
        h.outgoing(
            sender,
            Harness::mail(5),
            &json!({"to":{"agent":Harness::id(3)},"act":"message","subject":"valid"}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 2);
    }

    #[test]
    fn threaded_durable_prefix_resumes_after_sender_daily_allowance_is_full() {
        let mut h = Harness::new(3);
        let outbox = Harness::mail(1);
        let at = Timestamp::from_unix_millis(1);
        h.outgoing(
            Harness::id(1),
            outbox,
            &json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},
            "act":"message","subject":"prefix"}),
        );
        let writer = &mut h.writer;
        let mut appended = 0;
        h.mailbox.pass(
            &h.agents,
            || at,
            mint_thread,
            |record, admitted_at| {
                if appended == 1 {
                    anyhow::bail!("crash after first recipient");
                }
                let entry = Entry {
                    at: admitted_at.unwrap_or(at),
                    record: record.clone(),
                };
                let lsn = writer.append(&entry)?;
                appended += 1;
                Ok((lsn, entry))
            },
        );
        h.mailbox = Mailbox::replay(
            h.root.path().to_owned(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        for n in 100..131 {
            let entry = Entry { at, record: serde_json::from_value(json!({"t":"mail", "id":Harness::mail(n),
                "from":{"kind":"agent","agent":Harness::id(1)}, "to":{"kind":"agent","agent":Harness::id(2)},
                "act":"message", "subject":"allowance", "body_len":0, "state":{"state":"durable"}})).unwrap() };
            let lsn = h.writer.append(&entry).unwrap();
            h.mailbox.apply_entry(lsn, &entry);
        }
        assert_eq!(h.mailbox.rows.len(), 32);
        h.retire(Harness::id(3));
        h.pass_at(at);
        assert_eq!(h.mailbox.rows.len(), 33);
        let receipts: Vec<_> = h
            .mailbox
            .rows
            .values()
            .filter(|row| row.item.subject == "prefix")
            .collect();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].item.thread, receipts[1].item.thread);
        assert_eq!(receipts[0].item.thread_seq, 1);
        assert_eq!(receipts[1].item.thread_seq, 1);
    }

    #[test]
    fn reviewed_retry_rejects_same_inode_body_replacement_after_verification() {
        let mut h = Harness::new(3);
        let outbox = Harness::mail(1);
        let sender = Harness::id(1);
        let original = "x".repeat(5000);
        let replacement = "y".repeat(5000);
        let value = json!({"to":{"agents":[Harness::id(2),Harness::id(3)]},"act":"message","subject":"prefix","body":original});
        h.outgoing(sender, outbox, &value);
        let writer = &mut h.writer;
        let mut appended = false;
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |record, admitted_at| {
                if appended {
                    anyhow::bail!("crash after first receipt");
                }
                let entry = Entry {
                    at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                    record: record.clone(),
                };
                let lsn = writer.append(&entry)?;
                appended = true;
                Ok((lsn, entry))
            },
        );
        h.mailbox = Mailbox::replay(
            h.root.path().to_owned(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        let missing = dudes_journal::mail_recipient_id(outbox, Harness::id(3));
        std::fs::remove_file(h.path(
            Harness::id(3),
            "inbox/.staging",
            &format!("{missing}.body.tmp"),
        ))
        .unwrap();
        let mut retry = value;
        retry["body"] = json!(replacement);
        h.outgoing(sender, outbox, &retry);
        let outgoing: Outgoing = serde_json::from_value(retry).unwrap();
        let route = Route::parse(outgoing.to.as_ref(), outgoing.in_reply_to).unwrap();
        let plan = h
            .mailbox
            .mail_index
            .plan(sender, outbox, route, &h.mailbox.rows, &mut mint_thread)
            .unwrap();
        let tree = Tree::open(h.root.path(), sender, Opening::default()).unwrap();
        let prepared =
            h.mailbox
                .prepare_fanout_with(&tree, outbox, &outgoing, &plan, |tree, receipt| {
                    let path = tree.root_path.join(receipt.body_path.as_ref().unwrap());
                    let inode = std::fs::metadata(&path)?.ino();
                    std::fs::write(&path, &replacement)?;
                    assert_eq!(std::fs::metadata(&path)?.ino(), inode);
                    Ok(())
                });
        if let Ok(prepared) = prepared {
            let writer = &mut h.writer;
            for (item, mut staged_body) in prepared {
                if let Some(body) = &mut staged_body {
                    body.submitted();
                }
                h.mailbox
                    .persist(&Record::from(item), None, &mut |record, _admitted_at| {
                        let entry = Entry {
                            at: Timestamp::from_unix_millis(1),
                            record: record.clone(),
                        };
                        let lsn = writer.append(&entry)?;
                        Ok((lsn, entry))
                    });
            }
        }
        assert_eq!(h.mailbox.rows.len(), 1);
        assert_eq!(
            h.mailbox.rows.values().next().unwrap().item.body_blake3,
            Some(dudes_journal::mail_body_digest(original.as_bytes()))
        );
    }

    #[test]
    fn reviewed_admission_samples_after_midnight_io_with_yesterday_full() {
        for crossing in ["observe", "preflight"] {
            let mut h = Harness::new(2);
            let yesterday = Timestamp::from_unix_millis(86_399_999);
            let today = Timestamp::from_unix_millis(86_400_000);
            for n in 1..=32 {
                let entry = Entry { at: yesterday, record: serde_json::from_value(json!({"t":"mail", "id":Harness::mail(n),
                "from":{"kind":"agent","agent":Harness::id(1)}, "to":{"kind":"agent","agent":Harness::id(2)},
                "act":"message", "subject":"yesterday", "body_len":0, "state":{"state":"durable"}})).unwrap() };
                let lsn = h.writer.append(&entry).unwrap();
                h.mailbox.apply_entry(lsn, &entry);
            }
            h.outgoing(
            Harness::id(1),
            Harness::mail(33),
            &json!({"to":{"agent":Harness::id(2)},"act":"message","subject":"today","body":"x".repeat(5000)}),
        );
            let clock = Cell::new(yesterday);
            let writer = &mut h.writer;
            let mut append = |record: &Record, admitted_at: Option<Timestamp>| {
                let entry = Entry {
                    at: admitted_at.unwrap_or_else(|| clock.get()),
                    record: record.clone(),
                };
                let lsn = writer.append(&entry)?;
                Ok((lsn, entry))
            };
            if crossing == "observe" {
                h.mailbox.pass_with(
                    &h.agents,
                    || clock.get(),
                    mint_thread,
                    &mut append,
                    |_| clock.set(today),
                );
            } else {
                let tree = Tree::open(h.root.path(), Harness::id(1), Opening::default()).unwrap();
                let _ = h.mailbox.ingest_with(
                    &tree,
                    &mut || clock.get(),
                    &mut mint_thread,
                    &mut append,
                    |_| {
                        clock.set(today);
                        Ok(())
                    },
                );
            }
            assert_eq!(h.mailbox.rows.len(), 33);
            h.mailbox = Mailbox::replay(
                h.root.path().to_owned(),
                &Reader::new(&h.root.path().join("journal")),
            )
            .unwrap();
            for n in 34..=65 {
                h.outgoing(
                    Harness::id(1),
                    Harness::mail(n),
                    &json!({"to":{"agent":Harness::id(2)},"act":"message","subject":"today"}),
                );
                h.pass_at(today);
            }
            assert_eq!(h.mailbox.rows.len(), 64);
            let days: Vec<_> = Reader::new(&h.root.path().join("journal"))
                .iter_from(Lsn::ZERO)
                .unwrap()
                .map(Result::unwrap)
                .filter_map(|(_, entry)| {
                    matches!(entry.record, Record::Mail { .. }).then_some(entry.at.utc_day())
                })
                .collect();
            assert_eq!(
                days.iter()
                    .filter(|day| **day == yesterday.utc_day())
                    .count(),
                32
            );
            assert_eq!(
                days.iter().filter(|day| **day == today.utc_day()).count(),
                32
            );
        }
    }

    #[test]
    fn reviewed_daily_refusals_remove_unjournaled_body_stages() {
        let mut h = Harness::new(3);
        let sender = Harness::id(1);
        let recipient = Harness::id(2);
        for n in 1..=32 {
            let outbox = Harness::mail(n);
            h.send(sender, recipient, outbox, "admitted");
            h.settle();
            h.move_done(
                recipient,
                dudes_journal::mail_recipient_id(outbox, recipient),
            );
            h.settle();
        }
        assert_eq!(h.mailbox.rows.len(), 32);
        for n in 100..104 {
            let outbox = Harness::mail(n);
            h.send(sender, recipient, outbox, &"refused".repeat(1000));
            h.pass();
            assert!(h.path(sender, "outbox", &format!("{outbox}.json")).exists());
            std::fs::remove_file(h.path(sender, "outbox", &format!("{outbox}.json"))).unwrap();
        }
        let independent = Harness::mail(200);
        h.send(
            Harness::id(3),
            recipient,
            independent,
            &"legitimate".repeat(1000),
        );
        h.settle();
        let id = dudes_journal::mail_recipient_id(independent, recipient);
        assert_eq!(h.mailbox.rows.len(), 33);
        assert_eq!(
            std::fs::read_to_string(h.path(recipient, "inbox", &format!("{id}.body"))).unwrap(),
            "legitimate".repeat(1000)
        );
        assert_eq!(
            std::fs::read_dir(h.path(recipient, "inbox/.staging", ""))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn reviewed_unattempted_group_stages_roll_back_on_preflight_failure() {
        let mut h = Harness::new(3);
        let outbox = Harness::mail(1);
        let target = Harness::id(3);
        let collision = dudes_journal::mail_recipient_id(outbox, target);
        let path = h.path(target, "inbox/.staging", &format!("{collision}.body.tmp"));
        std::fs::write(&path, "unrelated collision").unwrap();
        h.outgoing(
            Harness::id(1),
            outbox,
            &json!({"to":{"agents":[Harness::id(2),target]},
            "act":"message","subject":"group","body":"prepared".repeat(1000)}),
        );
        h.pass();
        assert_eq!(h.mailbox.rows.len(), 0);
        assert_eq!(
            std::fs::read_dir(h.path(Harness::id(2), "inbox/.staging", ""))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unrelated collision"
        );
        std::fs::remove_file(&path).unwrap();
        h.settle();
        assert_eq!(h.mailbox.rows.len(), 2);
    }

    #[test]
    fn reviewed_ambiguous_mail_append_preserves_staged_body_for_replay() {
        let mut h = Harness::new(2);
        let outbox = Harness::mail(1);
        let recipient = Harness::id(2);
        let body = "durable".repeat(1000);
        h.send(Harness::id(1), recipient, outbox, &body);
        let writer = &mut h.writer;
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |record, admitted_at| {
                let entry = Entry {
                    at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                    record: record.clone(),
                };
                writer.append(&entry)?;
                anyhow::bail!("lost confirmation after durable Mail");
            },
        );
        let id = dudes_journal::mail_recipient_id(outbox, recipient);
        assert!(h.mailbox.fenced);
        assert!(
            h.path(recipient, "inbox/.staging", &format!("{id}.body.tmp"))
                .exists()
        );
        assert!(
            h.path(Harness::id(1), "outbox", &format!("{outbox}.json"))
                .exists()
        );
        h.mailbox = Mailbox::replay(
            h.root.path().to_owned(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert_eq!(h.mailbox.rows.len(), 1);
        assert_eq!(
            std::fs::read_to_string(h.path(recipient, "inbox", &format!("{id}.body"))).unwrap(),
            body
        );
    }

    #[test]
    fn continuing_arrivals_reserve_only_the_earliest_pending_slots() {
        let mut h = Harness::new(2);
        let sender = Harness::id(1);
        let recipient = Harness::id(2);
        for batch in 0..4 {
            for n in 1..=INBOX_CAP {
                h.send(
                    sender,
                    recipient,
                    Harness::mail((batch * INBOX_CAP + n) as u128),
                    "body",
                );
            }
            h.at = Timestamp::from_unix_millis(i64::try_from(batch).unwrap() * 86_400_000);
            h.settle();
            assert_eq!(h.json_count(recipient), INBOX_CAP);
        }
        assert_eq!(h.mailbox.rows.len(), INBOX_CAP * 4);
        let oldest = dudes_journal::mail_recipient_id(Harness::mail(1), recipient);
        h.move_done(recipient, oldest);
        h.settle();
        assert_eq!(h.acked(oldest), 1);
        assert!(
            h.path(
                recipient,
                "inbox",
                &format!(
                    "{}.json",
                    dudes_journal::mail_recipient_id(
                        Harness::mail(INBOX_CAP as u128 + 1),
                        recipient
                    )
                )
            )
            .exists()
        );
        assert_eq!(h.json_count(recipient), INBOX_CAP);
    }

    #[test]
    fn deferred_recipients_rotate_and_replay_rebuilds_rotation() {
        let mut h = Harness::new(RECIPIENTS_PER_PASS + 2);
        let recipient = Harness::id((RECIPIENTS_PER_PASS + 2) as u128);
        for n in 0..=INBOX_CAP {
            std::fs::write(
                h.path(Harness::id(1), "inbox/.done", &format!("noise-{n}")),
                "invalid",
            )
            .unwrap();
        }
        for n in 1..=RECIPIENTS_PER_PASS + 1 {
            h.send(
                Harness::id(n as u128),
                recipient,
                Harness::mail(n as u128),
                "body",
            );
        }
        h.pass();
        assert_eq!(h.mailbox.rows.len(), RECIPIENTS_PER_PASS);
        h.pass();
        assert_eq!(h.mailbox.rows.len(), RECIPIENTS_PER_PASS + 1);
        h.settle();
        assert_eq!(h.json_count(recipient), RECIPIENTS_PER_PASS + 1);
        let replay = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        assert_eq!(replay.next, None);
        assert_eq!(replay.rows.len(), RECIPIENTS_PER_PASS + 1);
    }

    #[test]
    fn complete_done_snapshot_defers_all_on_an_overflow_witness() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let marker = h.move_done(recipient, id);
        for n in 0..INBOX_CAP {
            std::fs::write(
                h.path(recipient, "inbox/.done", &format!("noise-{n}")),
                "invalid",
            )
            .unwrap();
        }
        h.settle();
        assert_eq!(h.acked(id), 0);
        assert!(marker.exists());
        assert_eq!(h.json_count(recipient), 0);
        std::fs::remove_file(h.path(recipient, "inbox/.done", "noise-0")).unwrap();
        h.settle();
        assert_eq!(h.acked(id), 1);
        assert!(!marker.exists());
    }

    #[test]
    fn case_hardlink_symlink_and_malformed_markers_never_republish() {
        for kind in ["case", "hardlink", "symlink", "malformed", "oversize"] {
            let mut h = Harness::new(2);
            let recipient = Harness::id(2);
            let id = Harness::mail(0xfeed);
            h.send(Harness::id(1), recipient, id, "body");
            let id = dudes_journal::mail_recipient_id(id, recipient);
            h.settle();
            let marker = h.move_done(recipient, id);
            match kind {
                "case" => {
                    std::fs::rename(
                        &marker,
                        marker.with_file_name(format!("{id}.json").to_lowercase()),
                    )
                    .unwrap();
                }
                "hardlink" => {
                    std::fs::hard_link(&marker, h.root.path().join("extra-link")).unwrap();
                }
                "symlink" => {
                    let outside = h.root.path().join("outside");
                    std::fs::rename(&marker, &outside).unwrap();
                    std::os::unix::fs::symlink(outside, &marker).unwrap();
                }
                "malformed" => {
                    std::fs::write(&marker, "{}").unwrap();
                }
                "oversize" => {
                    let mut bytes = std::fs::read(&marker).unwrap();
                    bytes.resize(usize::try_from(MARKER_BYTES).unwrap() + 1, b' ');
                    std::fs::write(&marker, bytes).unwrap();
                }
                _ => unreachable!(),
            }
            h.settle();
            assert_eq!(h.acked(id), 0, "{kind}");
            assert_eq!(h.json_count(recipient), 0, "{kind}");
        }
    }

    #[test]
    fn referenced_body_is_staged_before_mail_and_verified_before_ack() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        let body = "large".repeat(1000);
        h.send(Harness::id(1), recipient, id, &body);
        let id = dudes_journal::mail_recipient_id(id, recipient);
        let root = h.root.path().to_path_buf();
        let records = &mut h.records;
        let writer = &mut h.writer;
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |record, admitted_at| {
                if let Record::Mail {
                    body: inline,
                    body_path,
                    body_blake3,
                    ..
                } = record
                {
                    assert!(inline.is_none());
                    let path = root.join(body_path.as_ref().unwrap());
                    assert!(!path.exists(), "body publication follows the Mail append");
                    let staged = path
                        .parent()
                        .unwrap()
                        .join(".staging")
                        .join(format!("{id}.body.tmp"));
                    assert_eq!(std::fs::read(&staged).unwrap(), body.as_bytes());
                    assert_eq!(
                        body_blake3.as_ref().unwrap(),
                        &dudes_journal::mail_body_digest(body.as_bytes())
                    );
                }
                let lsn = writer.append(&Entry {
                    at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                    record: record.clone(),
                })?;
                records.push(record.clone());
                Ok((
                    lsn,
                    Entry {
                        at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                        record: record.clone(),
                    },
                ))
            },
        );
        h.settle();
        h.move_done(recipient, id);
        let body_path = h.path(recipient, "inbox", &format!("{id}.body"));
        std::fs::write(&body_path, "wrong").unwrap();
        h.settle();
        assert_eq!(h.acked(id), 0);
        assert_eq!(h.json_count(recipient), 0);
        std::fs::write(&body_path, body).unwrap();
        h.settle();
        assert_eq!(h.acked(id), 1);
    }

    #[test]
    fn reused_ids_preserve_original_facts_across_both_outboxes_and_replay() {
        let mut h = Harness::new(3);
        let id = Harness::mail(1);
        let recipient = Harness::id(2);
        let outbox = id;
        h.send(Harness::id(1), recipient, id, "original");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let original = h.mailbox.rows.get(&id).unwrap().item.clone();
        let delivered = std::fs::read(h.path(recipient, "inbox", &format!("{id}.json"))).unwrap();
        for sender in [Harness::id(1), Harness::id(3)] {
            h.send(sender, Harness::id(3), outbox, "replacement");
        }
        h.settle();
        assert_eq!(
            h.records
                .iter()
                .filter(|record| matches!(record, Record::Mail { id: found, .. } if *found == id))
                .count(),
            1
        );
        assert_eq!(h.mailbox.rows.get(&id).unwrap().item, original);
        assert_eq!(
            std::fs::read(h.path(recipient, "inbox", &format!("{id}.json"))).unwrap(),
            delivered
        );
        for sender in [Harness::id(1), Harness::id(3)] {
            assert!(h.path(sender, "outbox", &format!("{outbox}.json")).exists());
        }
        h.move_done(recipient, id);
        h.settle();
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert_eq!(h.mailbox.rows.get(&id).unwrap().item, original);
        assert_eq!(h.mailbox.rows.get(&id).unwrap().state, MailState::Acked);
        assert_eq!(h.json_count(recipient), 0);
    }

    #[test]
    fn retirement_bounces_new_mail_but_recovery_delivers_recorded_mail() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let old = Harness::mail(1);
        h.send(Harness::id(1), recipient, old, "old");
        let old = dudes_journal::mail_recipient_id(old, recipient);
        h.pass();
        h.retire(recipient);
        let new = Harness::mail(2);
        h.send(Harness::id(1), recipient, new, "new");
        let new = dudes_journal::mail_recipient_id(new, recipient);
        h.settle();
        assert!(h.path(recipient, "inbox", &format!("{old}.json")).exists());
        assert!(!h.path(recipient, "inbox", &format!("{new}.json")).exists());
        assert!(matches!(
            h.mailbox.rows.get(&new).unwrap().state,
            MailState::Bounced { .. }
        ));
        let unknown = Harness::mail(3);
        h.send(Harness::id(1), Harness::id(99), unknown, "unknown");
        let unknown = dudes_journal::mail_recipient_id(unknown, Harness::id(99));
        h.settle();
        assert!(matches!(
            h.mailbox.rows.get(&unknown).unwrap().state,
            MailState::Bounced { .. }
        ));
    }

    #[test]
    fn an_uncertain_append_retains_the_marker_and_fences_all_work_until_replay() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let marker = h.move_done(recipient, id);
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |_, _| anyhow::bail!("injected uncertain append"),
        );
        assert!(h.mailbox.fenced);
        assert!(marker.exists());
        h.send(Harness::id(1), recipient, Harness::mail(2), "new");
        h.settle();
        assert_eq!(h.acked(id), 0);
        assert_eq!(h.mailbox.rows.len(), 1);
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert_eq!(h.acked(id), 1);
        assert!(!marker.exists());
        assert_eq!(h.mailbox.rows.len(), 2);
    }

    #[test]
    fn a_directory_replacement_or_path_replacement_is_deferred() {
        let mut h = Harness::new(2);
        let tree = Tree::open(h.root.path(), Harness::id(2), Opening::default()).unwrap();
        let done = h.path(tree.id, "inbox/.done", "");
        std::fs::rename(&done, done.with_file_name("old-done")).unwrap();
        std::fs::create_dir(&done).unwrap();
        assert!(tree.validate().is_err());
        let name = OsStr::new("marker");
        std::fs::write(done.join(name), "first").unwrap();
        let new_tree = Tree::open(h.root.path(), tree.id, Opening::default()).unwrap();
        let (handle, _) = read_file(&new_tree.done, name, MARKER_BYTES).unwrap();
        std::fs::rename(done.join(name), done.join("retained")).unwrap();
        std::fs::write(done.join(name), "second").unwrap();
        assert!(validate_file(&new_tree.done, name, &handle).is_err());
        h.settle();
    }
    #[test]
    fn every_directory_component_requires_its_exact_raw_spelling() {
        for component in ["agents", "agent", "inbox", ".done", ".staging", "outbox"] {
            let root = tempfile::tempdir().unwrap();
            let id = AgentId::from_parts(1, 0xfeed);
            let tree = Tree::open(root.path(), id, Opening::default()).unwrap();
            let path = match component {
                "agents" => root.path().join("agents"),
                "agent" => root.path().join("agents").join(id.to_string()),
                "inbox" | "outbox" => root
                    .path()
                    .join("agents")
                    .join(id.to_string())
                    .join(component),
                _ => root
                    .path()
                    .join("agents")
                    .join(id.to_string())
                    .join("inbox")
                    .join(component),
            };
            let alias = if component == "agent" {
                id.to_string().to_lowercase()
            } else {
                component.to_uppercase()
            };
            std::fs::rename(&path, path.with_file_name(alias)).unwrap();
            assert!(tree.validate().is_err(), "{component}");
            assert!(
                Tree::open(root.path(), id, Opening::default()).is_err(),
                "{component}"
            );
        }
    }

    #[test]
    fn a_case_alias_body_cannot_ack_or_replace_the_recorded_body() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(0xfeed);
        h.send(Harness::id(1), recipient, id, &"body".repeat(1500));
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        h.move_done(recipient, id);
        let body = h.path(recipient, "inbox", &format!("{id}.body"));
        let alias = body.with_file_name(format!("{id}.body").to_lowercase());
        std::fs::rename(&body, &alias).unwrap();
        h.settle();
        assert_eq!(h.acked(id), 0);
        assert_eq!(h.json_count(recipient), 0);
        assert!(alias.exists());
    }

    #[test]
    fn a_durable_ack_crash_replays_cleanup_without_a_second_ack() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let marker = h.move_done(recipient, id);
        let writer = &mut h.writer;
        let records = &mut h.records;
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |record, admitted_at| {
                writer.append(&Entry {
                    at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                    record: record.clone(),
                })?;
                records.push(record.clone());
                anyhow::bail!("lost acknowledgement append confirmation");
            },
        );
        assert!(marker.exists());
        assert_eq!(h.acked(id), 1);
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert!(!marker.exists());
        assert_eq!(h.acked(id), 1);
        assert_eq!(h.json_count(recipient), 0);
    }

    #[test]
    fn a_pre_mail_crash_reuses_only_identical_durable_staging() {
        let mut h = Harness::new(2);
        let sender = Harness::id(1);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        let body = "body".repeat(1500);
        h.send(sender, recipient, id, &body);
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.mailbox.pass(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |_, _| anyhow::bail!("crash before Mail append"),
        );
        assert_eq!(h.mailbox.rows.len(), 0);
        assert!(
            h.path(recipient, "inbox/.staging", &format!("{id}.body.tmp"))
                .exists()
        );
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert_eq!(h.mailbox.rows.len(), 1);
        assert_eq!(
            std::fs::read(h.path(recipient, "inbox", &format!("{id}.body"))).unwrap(),
            body.as_bytes()
        );
        assert!(h.path(recipient, "inbox", &format!("{id}.json")).exists());
    }

    #[test]
    fn diagnostic_labels_and_rows_are_bounded_without_reading_unmatched_content() {
        let mut h = Harness::new(5);
        for agent in h.agents.keys().copied() {
            for n in 0..INBOX_CAP {
                let label = if n == 0 {
                    format!("{}.json", Harness::mail(0xffff))
                } else {
                    format!("{n}-{}", "名".repeat(64))
                };
                std::fs::write(
                    h.path(agent, "inbox/.done", &label),
                    "secret unmatched bytes",
                )
                .unwrap();
            }
        }
        h.pass();
        let report = h.mailbox.report.lock().unwrap();
        assert_eq!(report.diagnostics.len(), DIAGNOSTICS_CAP);
        assert!(report.truncated);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|row| row.problem == MailboxProblem::UnmatchedAcknowledgement)
        );
        assert!(
            report
                .diagnostics
                .iter()
                .all(|row| row.filename.len() <= LABEL_BYTES)
        );
        assert!(
            !serde_json::to_string(&*report)
                .unwrap()
                .contains("secret unmatched bytes")
        );
    }
    #[test]
    fn replacement_between_staging_verification_and_rename_is_refused() {
        let h = Harness::new(2);
        let recipient = Harness::id(2);
        let tree = Tree::open(h.root.path(), recipient, Opening::default()).unwrap();
        let stage = h.path(recipient, "inbox/.staging", "delivery.body.tmp");
        let retained = h.root.path().join("retained-stage");
        let result = publish_with(&tree, "delivery.body", b"original", || {
            std::fs::rename(&stage, &retained)?;
            std::fs::write(&stage, "replacement")
        });
        assert!(result.is_err());
        assert!(!h.path(recipient, "inbox", "delivery.body").exists());
        assert_eq!(std::fs::read(&retained).unwrap(), b"original");
        assert_eq!(std::fs::read(&stage).unwrap(), b"replacement");
    }

    #[test]
    fn historical_referenced_bodies_do_not_limit_continuing_mail_arrivals() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        for n in 1..=80 {
            let id = Harness::mail(n);
            h.send(Harness::id(1), recipient, id, &"body".repeat(1500));
            let id = dudes_journal::mail_recipient_id(id, recipient);
            h.at = Timestamp::from_unix_millis(i64::try_from(n).unwrap() * 86_400_000);
            h.pass();
            h.move_done(recipient, id);
            h.pass();
            assert_eq!(h.acked(id), 1);
        }
        assert_eq!(h.json_count(recipient), 0);
        assert!(
            std::fs::read_dir(h.path(recipient, "inbox", ""))
                .unwrap()
                .count()
                > 72
        );
        assert!(
            dudes_journal::body_integrity(
                &Reader::new(&h.root.path().join("journal")),
                h.root.path()
            )
            .unwrap()
            .is_empty()
        );
    }
    #[test]
    fn a_source_case_only_rename_after_decode_and_staging_never_appends() {
        let mut h = Harness::new(2);
        let sender = Harness::id(1);
        let id = Harness::mail(0xfeed);
        h.send(sender, Harness::id(2), id, &"body".repeat(1500));
        let tree = Tree::open(h.root.path(), sender, Opening::default()).unwrap();
        let outbox = h.path(sender, "outbox", "");
        let mut appended = 0;
        let _ = h.mailbox.ingest_with(
            &tree,
            &mut || Timestamp::from_unix_millis(1),
            &mut mint_thread,
            &mut |record, admitted_at| {
                appended += 1;
                Ok((
                    Lsn(1),
                    Entry {
                        at: admitted_at.unwrap_or(Timestamp::from_unix_millis(1)),
                        record: record.clone(),
                    },
                ))
            },
            |name| {
                std::fs::rename(
                    outbox.join(name),
                    outbox.join(name.to_str().unwrap().to_lowercase()),
                )
            },
        );
        assert_eq!(appended, 0);
        assert!(h.mailbox.rows.is_empty());
    }

    #[test]
    fn a_case_only_rename_after_the_byte_read_is_refused() {
        let h = Harness::new(2);
        let tree = Tree::open(h.root.path(), Harness::id(1), Opening::default()).unwrap();
        let id = Harness::mail(0xfeed);
        let name = format!("{id}.json");
        let path = h.path(tree.id, "outbox", &name);
        std::fs::write(&path, "bytes").unwrap();
        let result = read_file_with(&tree.outbox, OsStr::new(&name), OUTBOX_BYTES, || {
            std::fs::rename(&path, path.with_file_name(name.to_lowercase()))
        });
        assert!(result.is_err());
    }

    #[test]
    fn publication_refuses_a_case_only_staging_rename() {
        let h = Harness::new(2);
        let recipient = Harness::id(2);
        let tree = Tree::open(h.root.path(), recipient, Opening::default()).unwrap();
        let path = h.path(recipient, "inbox/.staging", "DELIVERY.body.tmp");
        let result = publish_with(&tree, "DELIVERY.body", b"body", || {
            std::fs::rename(&path, path.with_file_name("delivery.body.tmp"))
        });
        assert!(result.is_err());
        assert!(!h.path(recipient, "inbox", "DELIVERY.body").exists());
    }

    #[test]
    fn bounced_large_bodies_remain_whole_at_the_recorded_path_after_replay() {
        let mut h = Harness::new(2);
        let retired = Harness::id(2);
        h.retire(retired);
        for (n, recipient) in [(1, retired), (2, Harness::id(99))] {
            let id = Harness::mail(n);
            h.send(Harness::id(1), recipient, id, &"body".repeat(1500));
            let id = dudes_journal::mail_recipient_id(id, recipient);
            h.settle();
            assert!(matches!(
                h.mailbox.rows[&id].state,
                MailState::Bounced { .. }
            ));
            assert!(!h.path(recipient, "inbox", &format!("{id}.json")).exists());
            assert!(h.path(recipient, "inbox", &format!("{id}.body")).exists());
        }
        let reader = Reader::new(&h.root.path().join("journal"));
        assert!(
            dudes_journal::body_integrity(&reader, h.root.path())
                .unwrap()
                .is_empty()
        );
        h.mailbox = Mailbox::replay(h.root.path().to_path_buf(), &reader).unwrap();
        h.settle();
        assert!(
            dudes_journal::body_integrity(&reader, h.root.path())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn ineligible_acceptance_survives_body_collision_restart_and_later_hire() {
        for retired in [false, true] {
            let mut h = Harness::new(2);
            let recipient = if retired {
                Harness::id(2)
            } else {
                Harness::id(99)
            };
            if retired {
                h.retire(recipient);
            }
            let id = Harness::mail(1);
            let tree = Tree::open(h.root.path(), recipient, Opening::default()).unwrap();
            let receipt = dudes_journal::mail_recipient_id(id, recipient);
            let body = h.path(recipient, "inbox", &format!("{receipt}.body"));
            std::fs::write(&body, "collision").unwrap();
            h.send(Harness::id(1), recipient, id, &"body".repeat(1500));
            let id = dudes_journal::mail_recipient_id(id, recipient);
            h.settle();
            assert_eq!(h.mailbox.rows[&id].state, MailState::Durable);
            assert!(h.mailbox.rows[&id].bounce);
            assert_eq!(std::fs::read_to_string(&body).unwrap(), "collision");
            assert_eq!(h.json_count(recipient), 0);
            if !retired {
                let record: Record = serde_json::from_value(json!({"t":"agent_hired", "id":recipient, "name":"later", "role":"worker", "department":"test", "provider":"claude", "cwd":h.root.path(), "budget":{"usd_per_day":null,"tokens_per_day":null}})).unwrap();
                h.writer
                    .append(&Entry {
                        at: Timestamp::from_unix_millis(1),
                        record,
                    })
                    .unwrap();
                let row = h.agents[&Harness::id(1)];
                h.agents.insert(recipient, row);
            }
            h.mailbox = Mailbox::replay(
                h.root.path().to_path_buf(),
                &Reader::new(&h.root.path().join("journal")),
            )
            .unwrap();
            std::fs::remove_file(&body).unwrap();
            h.settle();
            assert!(matches!(
                h.mailbox.rows[&id].state,
                MailState::Bounced { .. }
            ));
            assert_eq!(h.json_count(recipient), 0);
            assert!(
                dudes_journal::body_integrity(
                    &Reader::new(&h.root.path().join("journal")),
                    h.root.path()
                )
                .unwrap()
                .is_empty()
            );
            tree.validate().unwrap();
        }
    }
    #[test]
    fn an_ineligible_marker_beyond_bounce_retry_cap_never_acks() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        h.retire(recipient);
        let body = "body".repeat(1500);
        for n in 1..=INBOX_CAP + 1 {
            let id = Harness::mail(n as u128);
            std::fs::write(
                h.path(
                    recipient,
                    "inbox",
                    &format!("{}.body", dudes_journal::mail_recipient_id(id, recipient)),
                ),
                "collision",
            )
            .unwrap();
            h.send(Harness::id(1), recipient, id, &body);

            if n == INBOX_CAP {
                h.settle();
            }
        }
        h.at = Timestamp::from_unix_millis(86_400_000);
        h.settle();
        let id = dudes_journal::mail_recipient_id(Harness::mail(INBOX_CAP as u128 + 1), recipient);
        std::fs::write(h.path(recipient, "inbox", &format!("{id}.body")), body).unwrap();
        let marker = h.path(recipient, "inbox/.done", &format!("{id}.json"));
        std::fs::write(
            &marker,
            serde_json::to_vec(&h.mailbox.rows[&id].item).unwrap(),
        )
        .unwrap();
        h.settle();
        assert_eq!(h.acked(id), 0);
        assert!(marker.exists());
        assert!(!h.path(recipient, "inbox", &format!("{id}.json")).exists());
        assert!(h.mailbox.rows[&id].bounce);
    }
    #[test]
    fn doctor_read_during_scan_keeps_the_last_completed_report() {
        let mut h = Harness::new(1);
        let agent = Harness::id(1);
        let old = h.path(agent, "inbox/.done", "old-marker");
        std::fs::write(&old, "unmatched").unwrap();
        h.pass();
        let published = h.mailbox.report();
        let previous = published.lock().unwrap().clone();
        assert_eq!(previous.diagnostics.len(), 1);
        std::fs::remove_file(old).unwrap();
        std::fs::write(h.path(agent, "inbox/.done", "new-marker"), "unmatched").unwrap();
        let mut paused = false;
        h.mailbox.pass_with(
            &h.agents,
            || Timestamp::from_unix_millis(1),
            mint_thread,
            |_, _| unreachable!(),
            |_| {
                paused = true;
                assert_eq!(*published.lock().unwrap(), previous);
            },
        );
        assert!(paused);
        assert_eq!(
            published.lock().unwrap().diagnostics[0].filename,
            "new-marker"
        );
    }
    #[test]
    fn unicode_case_alias_markers_never_ack_or_republish_json() {
        for (random, character, alias) in [(19, 'K', '\u{212a}'), (25, 'S', '\u{17f}')] {
            let mut h = Harness::new(2);
            let recipient = Harness::id(2);
            let id = Harness::mail(random);
            h.send(Harness::id(1), recipient, id, "body");
            let id = dudes_journal::mail_recipient_id(id, recipient);
            h.settle();
            let marker = h.move_done(recipient, id);
            let canonical_name = format!("{id}.json");
            assert!(canonical_name.contains(character));
            let alias_name = canonical_name.replace(character, &alias.to_string());
            let aliased = marker.with_file_name(&alias_name);
            std::fs::rename(&marker, &aliased).unwrap();
            assert!(
                std::fs::read_dir(marker.parent().unwrap())
                    .unwrap()
                    .any(|entry| entry.unwrap().file_name() == OsStr::new(&alias_name))
            );
            #[cfg(target_os = "macos")]
            assert!(
                std::fs::metadata(&marker).is_ok(),
                "the host proves a native Unicode path alias"
            );
            h.settle();
            assert_eq!(h.acked(id), 0);
            assert!(aliased.exists());
            assert_eq!(h.json_count(recipient), 0);
        }
    }

    #[test]
    fn an_acknowledgement_between_the_checks_is_delivered_once() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let name = format!("{id}.json");
        let delivered = h.path(recipient, "inbox", &name);
        let marker = h.path(recipient, "inbox/.done", &name);
        assert!(delivered.exists());
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        let tree = Tree::open(h.root.path(), recipient, Opening::default()).unwrap();
        let mut moved = 0;
        let _ = h.mailbox.deliver_with(&tree, |found| {
            assert_eq!(found, name);
            std::fs::rename(&delivered, &marker).unwrap();
            moved += 1;
        });
        assert_eq!(moved, 1);
        assert!(
            !delivered.exists(),
            "the acknowledged item was published again"
        );
        assert!(
            !h.path(recipient, "inbox/.staging", &format!("{name}.tmp"))
                .exists(),
            "the refused publication left its staged file"
        );
        h.settle();
        assert_eq!(h.acked(id), 1);
        assert!(!marker.exists());
        assert_eq!(h.json_count(recipient), 0);
    }

    #[test]
    fn unknown_recipients_leave_the_rotation_without_directories() {
        let mut h = Harness::new(2);
        let (sender, recipient) = (Harness::id(1), Harness::id(2));
        for batch in 0..2u128 {
            for n in 0..OUTBOX_SCAN as u128 {
                let k = batch * OUTBOX_SCAN as u128 + n;
                h.send(sender, Harness::id(100 + k), Harness::mail(k), "small");
            }
            h.at = Timestamp::from_unix_millis(i64::try_from(batch).unwrap() * 86_400_000);
            h.settle();
        }
        for k in 0..2 * OUTBOX_SCAN as u128 {
            assert!(matches!(
                h.mailbox.rows
                    [&dudes_journal::mail_recipient_id(Harness::mail(k), Harness::id(100 + k))]
                    .state,
                MailState::Bounced { .. }
            ));
            assert!(
                !h.root
                    .path()
                    .join("agents")
                    .join(Harness::id(100 + k).to_string())
                    .exists(),
                "an unknown recipient got a directory"
            );
        }
        h.at = Timestamp::from_unix_millis(2 * 86_400_000);
        let id = Harness::mail(1000);
        h.send(sender, recipient, id, "real");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.pass();
        h.pass();
        assert!(h.path(recipient, "inbox", &format!("{id}.json")).exists());
    }

    #[test]
    fn a_published_item_is_verified_once_per_run() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        let name = format!("{id}.json");
        std::fs::write(h.path(recipient, "inbox", &name), "rewritten by the agent").unwrap();
        h.settle();
        assert!(!h.diagnosed(recipient, &name, MailboxProblem::DeliveryCollision));
        h.mailbox = Mailbox::replay(
            h.root.path().to_path_buf(),
            &Reader::new(&h.root.path().join("journal")),
        )
        .unwrap();
        h.settle();
        assert!(h.diagnosed(recipient, &name, MailboxProblem::DeliveryCollision));
    }

    #[test]
    fn a_pass_over_published_items_issues_no_fsync() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        for n in 1..=INBOX_CAP {
            h.send(Harness::id(1), recipient, Harness::mail(n as u128), "body");
        }
        h.settle();
        assert_eq!(h.json_count(recipient), INBOX_CAP);
        let budget = Rc::new(Budget::default());
        h.pass_within(&budget);
        assert_eq!(budget.fsyncs.get(), 0);
    }

    #[test]
    fn one_pass_stops_starting_work_at_its_fsync_budget() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        for n in 1..=INBOX_CAP {
            h.send(Harness::id(1), recipient, Harness::mail(n as u128), "body");
        }
        let mut passes = Vec::new();
        while h.json_count(recipient) < INBOX_CAP {
            assert!(passes.len() < 64, "delivery stalled after {passes:?}");
            let budget = Rc::new(Budget::default());
            h.pass_within(&budget);
            passes.push(budget.fsyncs.get());
        }
        assert!(passes.len() > 2, "{passes:?}");
        assert!(
            passes.iter().all(|fsyncs| *fsyncs <= PASS_FSYNCS + 8),
            "{passes:?}"
        );
    }

    #[test]
    fn inbox_entries_past_the_scan_cap_defer_the_recipient() {
        let mut h = Harness::new(2);
        let recipient = Harness::id(2);
        for n in 0..2 * SCAN_CAP {
            std::fs::write(h.path(recipient, "inbox", &format!("junk-{n}")), "").unwrap();
        }
        let id = Harness::mail(1);
        h.send(Harness::id(1), recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        assert!(h.diagnosed(recipient, "inbox", MailboxProblem::ScanOverflow));
        assert!(!h.path(recipient, "inbox", &format!("{id}.json")).exists());
        let budget = Rc::new(Budget::default());
        h.pass_within(&budget);
        assert!(budget.entries.get() < 2 * SCAN_CAP, "{budget:?}");
        for n in 0..2 * SCAN_CAP {
            std::fs::remove_file(h.path(recipient, "inbox", &format!("junk-{n}"))).unwrap();
        }
        h.settle();
        assert!(h.path(recipient, "inbox", &format!("{id}.json")).exists());
        assert!(!h.diagnosed(recipient, "inbox", MailboxProblem::ScanOverflow));
    }

    #[test]
    fn recorded_bodies_do_not_count_toward_the_scan_cap() {
        let mut h = Harness::new(2);
        let (sender, recipient) = (Harness::id(1), Harness::id(2));
        h.record_history(recipient, SCAN_CAP as u64 + 16);
        let id = Harness::mail(1);
        h.send(sender, recipient, id, "body");
        let id = dudes_journal::mail_recipient_id(id, recipient);
        h.settle();
        assert!(h.path(recipient, "inbox", &format!("{id}.json")).exists());
        assert!(!h.diagnosed(recipient, "inbox", MailboxProblem::ScanOverflow));
    }

    #[test]
    fn an_oversized_item_to_an_unknown_recipient_creates_nothing() {
        let mut h = Harness::new(1);
        let (sender, unknown) = (Harness::id(1), Harness::id(77));
        let id = Harness::mail(1);
        std::fs::write(
            h.path(sender, "outbox", &format!("{id}.json")),
            serde_json::to_vec(&json!({
                "to": {"kind": "agent", "agent": unknown}, "act": "message",
                "subject": "s".repeat(usize::try_from(MARKER_BYTES).unwrap()),
                "body": "body".repeat(1500)
            }))
            .unwrap(),
        )
        .unwrap();
        h.settle();
        assert!(h.diagnosed(
            sender,
            &format!("{id}.json"),
            MailboxProblem::OutboxCorruption
        ));
        assert!(h.mailbox.rows.is_empty());
        assert!(
            !h.root
                .path()
                .join("agents")
                .join(unknown.to_string())
                .exists(),
            "an oversized item created directories"
        );
    }
}
