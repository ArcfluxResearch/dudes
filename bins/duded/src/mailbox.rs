#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::cell::RefCell;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeMap;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_journal::Reader;
use dudes_proto::rpc::system::MailboxReport;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_proto::rpc::system::{MailboxDiagnostic, MailboxProblem};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_proto::{Entry, Record};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_proto::{MailAct, MailFrom, MailTo};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_types::MailState;
use dudes_types::{AgentId, MailId};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use dudes_types::{Lsn, ThreadId, Timestamp};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const INBOX_CAP: usize = 32;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const RECIPIENTS_PER_PASS: usize = 8;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const DIAGNOSTICS_CAP: usize = 128;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const LABEL_BYTES: usize = 128;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Item {
    id: MailId,
    from: MailFrom,
    to: MailTo,
    act: MailAct,
    subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    body_len: u64,
    #[serde(default)]
    thread: Option<ThreadId>,
    #[serde(default)]
    thread_seq: u32,
    #[serde(default)]
    members: Vec<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_blake3: Option<String>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl From<Item> for Record {
    fn from(item: Item) -> Self {
        Self::Mail {
            id: item.id,
            from: item.from,
            to: item.to,
            act: item.act,
            subject: item.subject,
            subject_admission: Some(1),
            body: item.body,
            body_len: item.body_len,
            body_path: item.body_path,
            body_blake3: item.body_blake3,
            thread: item.thread,
            thread_seq: item.thread_seq,
            members: item.members,
            state: MailState::Durable,
            actor: None,
            provenance: None,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Debug)]
struct Row {
    item: Item,
    subject_admission: Option<u32>,
    lsn: Lsn,
    state: MailState,
    bounce: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnreadObservation {
    Complete(u32),
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Incomplete,
}

#[derive(Debug)]
pub(crate) struct OutboxGrant {
    path: PathBuf,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    identity: unix::OutboxIdentity,
}

impl OutboxGrant {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn check(&self) -> std::io::Result<()> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        return self.identity.check(&self.path).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("Codex outbox {} was refused: {error}", self.path.display()),
            )
        });
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "file mail {} is unavailable on this platform",
                self.path.display()
            ),
        ))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) fn outbox_fixture(root: &Path, agent: AgentId) -> Arc<OutboxGrant> {
    unix::outbox_fixture(root, agent)
}

pub(crate) struct Mailbox {
    private_roots: Arc<crate::codex_profile::scoped::PrivateRoots>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    root: PathBuf,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    rows: BTreeMap<MailId, Row>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mail_index: index::MailIndex,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    subject_contexts: BTreeMap<AgentId, unix::SubjectContext>,
    /// The `Durable` and `Notified` rows of each recipient, in original lsn order. Its recipients
    /// join the rotation, and its order assigns the inbox slots.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pending: BTreeMap<AgentId, BTreeSet<(Lsn, MailId)>>,
    /// Recorded out-of-line bodies per recipient. Each one stays in that recipient's inbox for good,
    /// so a directory search allows for them on top of its cap.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    bodies: BTreeMap<AgentId, usize>,
    /// Hire records seen, an upper bound on the agent directories a search may meet.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    hired: usize,
    /// Items this daemon run published and verified. Replay starts it empty, so an item already in
    /// an inbox is verified and repaired once per run and never again in the same run.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    published: BTreeSet<MailId>,
    /// Visits this run has paid to each recipient in the rotation.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    visits: BTreeMap<AgentId, usize>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    live_agents: BTreeSet<AgentId>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    outbox_agents: BTreeSet<AgentId>,
    // Keep the first admitted directory identity for this daemon run. Periodic setup must not
    // refresh a missing or replaced grant; restart admits the current private tree afresh.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    outboxes: RefCell<BTreeMap<AgentId, Arc<OutboxGrant>>>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    next: Option<AgentId>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fenced: bool,
    report: Arc<Mutex<MailboxReport>>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    observation: RefCell<MailboxReport>,
}

impl Mailbox {
    pub(crate) fn coordinate_private_roots(
        &mut self,
        roots: Arc<crate::codex_profile::scoped::PrivateRoots>,
    ) {
        self.private_roots = roots;
    }
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        expect(
            clippy::unused_self,
            reason = "unsupported platforms share the mailbox refusal method"
        )
    )]
    pub(crate) fn admit_outbox(&self, agent: AgentId) -> std::io::Result<Arc<OutboxGrant>> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.unix_admit_outbox(agent)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = agent;
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }
    pub(crate) fn is_fenced(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.fenced
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            !self.report.lock().is_ok_and(|report| report.available)
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn notify_prepared(
        &mut self,
        agent: AgentId,
        root_lsn: Lsn,
        target: &dudes_proto::MailWakeTarget,
        mut append: impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fenced, "mailbox replay is required");
        let Some(row) = self.rows.get(&target.mail.id) else {
            anyhow::bail!("prepared receipt is unavailable");
        };
        let count = match row.state {
            MailState::Durable => Some(0),
            MailState::Notified { n } => Some(n),
            _ => None,
        };
        anyhow::ensure!(
            root_lsn > target.mail.lsn
                && row.lsn == target.mail.lsn
                && row.item.to == (MailTo::Agent { agent })
                && count == Some(target.original_count)
                && target.original_count < 5
                && target.target_count == target.original_count + 1,
            "prepared notification does not match canonical receipt"
        );
        let record = Record::MailState {
            id: target.mail.id,
            state: MailState::Notified {
                n: target.target_count,
            },
        };
        anyhow::ensure!(
            self.persist(&record, None, &mut append),
            "prepared notification append is unresolved"
        );
        Ok(())
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn replay(root: PathBuf, reader: &Reader) -> dudes_journal::Result<Self> {
        let mut mailbox = Self {
            private_roots: Arc::default(),
            root,
            rows: BTreeMap::new(),
            mail_index: index::MailIndex::default(),
            subject_contexts: BTreeMap::new(),
            pending: BTreeMap::new(),
            bodies: BTreeMap::new(),
            hired: 0,
            published: BTreeSet::new(),
            visits: BTreeMap::new(),
            live_agents: BTreeSet::new(),
            outbox_agents: BTreeSet::new(),
            outboxes: RefCell::default(),
            next: None,
            fenced: false,
            observation: RefCell::new(MailboxReport {
                available: true,
                ..MailboxReport::default()
            }),
            report: Arc::new(Mutex::new(MailboxReport {
                available: true,
                ..MailboxReport::default()
            })),
        };
        for entry in reader.iter_from(Lsn::ZERO)? {
            let (lsn, entry) = entry?;
            mailbox.apply_entry(lsn, &entry);
        }
        Ok(mailbox)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn unavailable() -> Self {
        Self {
            private_roots: Arc::default(),
            report: Arc::new(Mutex::new(MailboxReport {
                available: false,
                detail: cfg!(windows).then(|| "mail is not yet available on Windows".to_owned()),
                ..MailboxReport::default()
            })),
        }
    }

    pub(crate) fn unread_observation(mailbox: Option<&Self>, agent: AgentId) -> UnreadObservation {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            mailbox.map_or(UnreadObservation::Complete(0), |mailbox| {
                mailbox.unix_unread_observation(agent)
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (mailbox, agent);
            UnreadObservation::Complete(0)
        }
    }

    /// Which of the given mail items sit unread in the agent's inbox, or none when the mailbox
    /// cannot say.
    pub(crate) fn inbox_present(
        mailbox: Option<&Self>,
        agent: AgentId,
        ids: &[MailId],
    ) -> Option<Vec<MailId>> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            mailbox?.unix_inbox_present(agent, ids).ok()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (mailbox, agent, ids);
            None
        }
    }

    pub(crate) fn report(&self) -> Arc<Mutex<MailboxReport>> {
        Arc::clone(&self.report)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn contains(&self, id: MailId) -> bool {
        self.rows.contains_key(&id)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn apply_entry(&mut self, lsn: Lsn, entry: &Entry) {
        let record = &entry.record;
        if !matches!(record, Record::Mail { id, .. } if self.rows.contains_key(id)) {
            self.mail_index.apply(entry);
        }
        match record {
            Record::AgentHired { id, provider, .. } => {
                self.live_agents.insert(*id);
                if *provider == dudes_types::Provider::Codex {
                    self.outbox_agents.insert(*id);
                }
                self.hired += 1;
            }
            Record::AgentHiredWithRepositories(hire)
            | Record::AgentHiredWithUnconfinedRepositories(hire) => {
                self.live_agents.insert(hire.id);
                if matches!(
                    hire.provider,
                    dudes_types::Provider::Claude | dudes_types::Provider::Codex
                ) {
                    self.outbox_agents.insert(hire.id);
                }
                self.hired += 1;
            }
            Record::AgentRetired { id, .. } => {
                self.live_agents.remove(id);
                self.outbox_agents.remove(id);
                self.outboxes.borrow_mut().remove(id);
                self.subject_contexts.remove(id);
            }
            Record::Detached { agent, .. } => {
                self.subject_contexts.remove(agent);
            }
            Record::TurnEnded { agent, turn, .. } => {
                if self
                    .subject_contexts
                    .get(agent)
                    .is_some_and(|context| context.holder == dudes_supervisor::Holder::Turn(*turn))
                {
                    self.subject_contexts.remove(agent);
                }
            }
            Record::Mail {
                id,
                from,
                to,
                act,
                subject,
                subject_admission,
                body,
                body_len,
                body_path,
                body_blake3,
                state,
                thread,
                thread_seq,
                members,
                ..
            } => {
                let std::collections::btree_map::Entry::Vacant(vacant) = self.rows.entry(*id)
                else {
                    return;
                };
                let bounce =
                    matches!(to, MailTo::Agent { agent } if !self.live_agents.contains(agent));
                vacant.insert(Row {
                    subject_admission: *subject_admission,
                    item: Item {
                        id: *id,
                        from: *from,
                        to: *to,
                        act: *act,
                        subject: subject.clone(),
                        body: body.clone(),
                        body_len: *body_len,
                        thread: *thread,
                        thread_seq: *thread_seq,
                        members: members.clone(),
                        body_path: body_path.clone(),
                        body_blake3: body_blake3.clone(),
                    },
                    lsn,
                    state: state.clone(),
                    bounce,
                });
                if let (MailTo::Agent { agent }, Some(_)) = (to, body_path) {
                    *self.bodies.entry(*agent).or_default() += 1;
                }
                self.index(*id);
            }
            Record::MailState { id, state } => {
                if let Some(row) = self.rows.get_mut(id) {
                    row.state = state.clone();
                    self.index(*id);
                }
            }
            _ => {}
        }
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    fn apply(&mut self, lsn: Lsn, record: &Record) {
        self.apply_entry(
            lsn,
            &Entry {
                at: Timestamp::from_unix_millis(1),
                record: record.clone(),
            },
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn index(&mut self, id: MailId) {
        let row = &self.rows[&id];
        let MailTo::Agent { agent } = row.item.to else {
            return;
        };
        let key = (row.lsn, id);
        if matches!(row.state, MailState::Durable | MailState::Notified { .. }) {
            self.pending.entry(agent).or_default().insert(key);
            return;
        }
        self.published.remove(&id);
        if let Some(rows) = self.pending.get_mut(&agent) {
            rows.remove(&key);
            if rows.is_empty() {
                self.pending.remove(&agent);
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn diagnostic(&self, agent: AgentId, filename: &str, problem: MailboxProblem) {
        let mut report = self.observation.borrow_mut();
        let mut end = filename.len().min(LABEL_BYTES);
        while !filename.is_char_boundary(end) {
            end -= 1;
        }
        let row = MailboxDiagnostic {
            agent,
            filename: filename[..end].to_owned(),
            problem,
        };
        if report.diagnostics.contains(&row) {
            return;
        }
        if report.diagnostics.len() < DIAGNOSTICS_CAP {
            report.diagnostics.push(row);
        } else {
            report.truncated = true;
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn persist(
        &mut self,
        record: &Record,
        at: Option<Timestamp>,
        append: &mut impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
    ) -> bool {
        match append(record, at) {
            Ok((lsn, entry)) => {
                self.apply_entry(lsn, &entry);
                true
            }
            Err(error) => {
                self.fenced = true;
                tracing::error!(%error, "mailbox append failed; replay is required");
                false
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn pass(
        &mut self,
        agents: &BTreeMap<AgentId, bool>,
        now: impl FnMut() -> Timestamp,
        mint: impl FnMut() -> ThreadId,
        append: impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
    ) {
        self.pass_with(agents, now, mint, append, |_| {});
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn pass_with(
        &mut self,
        agents: &BTreeMap<AgentId, bool>,
        mut now: impl FnMut() -> Timestamp,
        mut mint: impl FnMut() -> ThreadId,
        mut append: impl FnMut(&Record, Option<Timestamp>) -> anyhow::Result<(Lsn, Entry)>,
        before_observe: impl FnMut(AgentId),
    ) {
        if self.fenced {
            return;
        }
        *self.observation.borrow_mut() = self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.unix_pass(
            agents,
            &mut now,
            &mut mint,
            &mut append,
            before_observe,
            &Rc::default(),
        );
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.observation.borrow().clone();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod index;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod wake_tests {
    use super::*;
    use dudes_proto::{MailRef, MailWakeTarget};

    fn fixture(count: u8) -> (Mailbox, AgentId, MailWakeTarget) {
        let agent = AgentId::from_parts(1, 1);
        let id = MailId::from_parts(1, 2);
        let mut mailbox = Mailbox {
            private_roots: Arc::default(),
            root: PathBuf::new(),
            rows: BTreeMap::new(),
            mail_index: index::MailIndex::default(),
            subject_contexts: BTreeMap::new(),
            pending: BTreeMap::new(),
            bodies: BTreeMap::new(),
            hired: 0,
            published: BTreeSet::new(),
            visits: BTreeMap::new(),
            live_agents: BTreeSet::from([agent]),
            outbox_agents: BTreeSet::new(),
            outboxes: RefCell::default(),
            next: None,
            fenced: false,
            report: Arc::new(Mutex::new(MailboxReport::default())),
            observation: RefCell::new(MailboxReport::default()),
        };
        let state = if count == 0 {
            MailState::Durable
        } else {
            MailState::Notified { n: count }
        };
        let record = Record::from(Item {
            id,
            from: MailFrom::Human,
            to: MailTo::Agent { agent },
            act: MailAct::Message,
            subject: "private".into(),
            body: None,
            body_len: 0,
            thread: None,
            thread_seq: 0,
            members: Vec::new(),
            body_path: None,
            body_blake3: None,
        });
        mailbox.apply(Lsn(10), &record);
        if count != 0 {
            mailbox.apply(Lsn(11), &Record::MailState { id, state });
        }
        (
            mailbox,
            agent,
            MailWakeTarget {
                mail: MailRef { id, lsn: Lsn(10) },
                original_count: count,
                target_count: count + 1,
            },
        )
    }

    #[test]
    fn a_scoped_inspection_defers_private_mutations_without_waiting_for_the_worker() {
        let (mut mailbox, agent, _) = fixture(0);
        let root = tempfile::tempdir().unwrap();
        mailbox.root = root.path().canonicalize().unwrap();
        mailbox.outbox_agents.insert(agent);
        let path = mailbox.root.join("agents").join(agent.to_string());
        let roots = mailbox.private_roots.agent(&path).unwrap();
        let inspection = roots.inspect().unwrap();
        assert!(mailbox.admit_outbox(agent).is_err());
        assert!(!path.join("outbox").exists());
        assert_eq!(*inspection, 0);
        drop(inspection);
        mailbox.admit_outbox(agent).unwrap();
        assert!(path.join("outbox").is_dir());
        assert!(*roots.inspect().unwrap() > 0);
    }

    #[test]
    fn exact_prepared_advance_is_folded_once_and_terminal_mail_is_preserved() {
        for count in [0, 4] {
            let (mut mailbox, agent, target) = fixture(count);
            let mut calls = 0;
            mailbox
                .notify_prepared(agent, Lsn(20), &target, |record, _| {
                    calls += 1;
                    assert_eq!(
                        *record,
                        Record::MailState {
                            id: target.mail.id,
                            state: MailState::Notified { n: count + 1 }
                        }
                    );
                    Ok((
                        Lsn(21),
                        Entry {
                            at: Timestamp::UNIX_EPOCH,
                            record: record.clone(),
                        },
                    ))
                })
                .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(
                mailbox.rows[&target.mail.id].state,
                MailState::Notified { n: count + 1 }
            );
            assert!(
                mailbox
                    .notify_prepared(agent, Lsn(20), &target, |_, _| panic!("duplicate advance"))
                    .is_err()
            );
            mailbox.apply(
                Lsn(22),
                &Record::MailState {
                    id: target.mail.id,
                    state: MailState::Acked,
                },
            );
            assert!(
                mailbox
                    .notify_prepared(agent, Lsn(20), &target, |_, _| panic!("terminal overwrite"))
                    .is_err()
            );
            assert_eq!(mailbox.rows[&target.mail.id].state, MailState::Acked);
        }
    }

    #[test]
    fn stale_target_and_uncertain_append_never_fold_or_retry() {
        let (mut mailbox, agent, target) = fixture(0);
        let wrong = MailWakeTarget {
            mail: MailRef {
                lsn: Lsn(9),
                ..target.mail
            },
            ..target.clone()
        };
        assert!(
            mailbox
                .notify_prepared(agent, Lsn(20), &wrong, |_, _| panic!("wrong receipt"))
                .is_err()
        );
        assert!(
            mailbox
                .notify_prepared(agent, Lsn(10), &target, |_, _| panic!(
                    "root before receipt"
                ))
                .is_err()
        );
        assert!(!mailbox.is_fenced());
        assert!(
            mailbox
                .notify_prepared(agent, Lsn(20), &target, |_, _| Err(anyhow::anyhow!(
                    "fixture uncertain append"
                )))
                .is_err()
        );
        assert_eq!(mailbox.rows[&target.mail.id].state, MailState::Durable);
        assert!(mailbox.is_fenced());
        assert!(
            mailbox
                .notify_prepared(agent, Lsn(20), &target, |_, _| panic!("blind retry"))
                .is_err()
        );
    }
}
