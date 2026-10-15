//! The Floor's opt-in ticket diff route: the closed request, the binding to what the journal
//! recorded, and every limit that keeps one operator session from reading a checkout without end.
//!
//! The route is a pull: the Floor asks when the operator opens or refreshes the review. A
//! **manifest** request resolves the ticket the browser names, reads the checkout once, caches an
//! immutable list of changed files under server-minted ids, and answers its first page. A **page**
//! request continues that list. A **content** request returns up to eight complete files from it.
//! The browser never supplies a path, a revision or a pattern.
//!
//! Three limits are per session and survive CSRF rotation, reloads and reconnects, because the key
//! is the session cookie's digest. A session may start a read once per interval. It may receive
//! [`SESSION_ALLOWANCE`] bytes of successful replies in all, charged before the reply is sent and
//! never refunded. A reply that does not fit ends the allowance, so a smaller one cannot follow. It may hold one manifest, and the daemon holds at most [`MAX_MANIFESTS`].
//! Reads share one daemon-wide slot with the baseline and stamp captures.
//!
//! What the journal recorded decides everything the browser is told about freshness. A reply is
//! `unknown` unless the newest recorded stamp is complete, was taken by the sampler now running,
//! and the checkout still hashes to its digest.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use dudes_proto::diff_stamp::{BaselineOutcome, BaselinePayload, StampOutcome, StampPayload};
use dudes_proto::{
    CardView, DiffBinding, DiffFreshness, DiffRefusal, DiffResponse, MAX_DIFF_FILES, TicketRow,
};
use dudes_supervisor::World;
use dudes_types::{AgentId, CardId, GoalId, Lsn, Seq, Timestamp};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::floor_git::{Finish, GitTarget, Worker};
use crate::git_read::Tree;
use crate::git_read::diff::{
    BaselineCapture, CandidateManifest, DiffReader, RootIdentity, SAMPLER_VERSION,
    TEXT_POLICY_VERSION,
};

/// Most bytes one request body may carry, checked before a byte of it is read.
pub const MAX_DIFF_BODY_BYTES: usize = 1024;
/// How soon the same session may start another read.
pub const SESSION_INTERVAL: Duration = Duration::from_secs(2);
/// Bytes of successful replies one session may receive in all.
pub const SESSION_ALLOWANCE: usize = 1024 * 1024;
/// Manifests the daemon keeps at once, one per session at most.
pub const MAX_MANIFESTS: usize = 4;
/// How many abandoned workers the route tolerates before it answers `busy`.
pub const MAX_ABANDONED: usize = 2;
/// How long a session's totals are kept after its last request. A session lives at most eight
/// hours, so a longer-idle key can no longer present a cookie.
const KEEP: Duration = Duration::from_hours(9);
/// How often the waiting connection looks at its stop signal.
const POLL: Duration = Duration::from_millis(25);
/// What the connection allows beyond the reader's own deadline before it abandons a worker.
const GRACE: Duration = Duration::from_secs(2);

/// One bounded request. Every member is an opaque id the server minted or a card the journal
/// already knows.
#[derive(Clone, PartialEq, Eq)]
pub enum DiffRequest {
    /// Start a review of `card`, which must still be at `expected_seq`.
    Manifest {
        /// The ticket.
        card: CardId,
        /// The card sequence the browser last saw.
        expected_seq: Seq,
    },
    /// The next page of the session's manifest.
    Page {
        /// The manifest the session holds.
        manifest_id: String,
        /// A continuation that manifest minted.
        cursor: String,
    },
    /// Whole files from the session's manifest.
    Content {
        /// The manifest the session holds.
        manifest_id: String,
        /// One to eight entry ids that manifest minted.
        entries: Vec<String>,
    },
}

impl fmt::Debug for DiffRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiffRequest(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    Manifest {
        card: CardId,
        expected_seq: Seq,
    },
    Page {
        manifest_id: String,
        cursor: String,
    },
    Content {
        manifest_id: String,
        entries: Vec<String>,
    },
}

fn opaque(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

impl DiffRequest {
    /// Decode one body: one flat object of the mode's members and no others.
    ///
    /// # Errors
    ///
    /// A body over [`MAX_DIFF_BODY_BYTES`], one that is not an object, one with an unknown,
    /// missing or repeated member, an id that is not 32 lowercase hexadecimal characters, and a
    /// content request that names none or more than eight entries.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DIFF_BODY_BYTES {
            bail!("Floor diff request is too large");
        }
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor diff request is not one JSON object");
        }
        let request = match serde_json::from_slice::<Wire>(bytes)? {
            Wire::Manifest { card, expected_seq } => Self::Manifest { card, expected_seq },
            Wire::Page {
                manifest_id,
                cursor,
            } => Self::Page {
                manifest_id,
                cursor,
            },
            Wire::Content {
                manifest_id,
                entries,
            } => Self::Content {
                manifest_id,
                entries,
            },
        };
        let well_formed = match &request {
            Self::Manifest { .. } => true,
            Self::Page {
                manifest_id,
                cursor,
            } => opaque(manifest_id) && opaque(cursor),
            Self::Content {
                manifest_id,
                entries,
            } => {
                opaque(manifest_id)
                    && (1..=MAX_DIFF_FILES).contains(&entries.len())
                    && entries.iter().all(|entry| opaque(entry))
            }
        };
        if !well_formed {
            bail!("Floor diff request names a malformed id");
        }
        Ok(request)
    }
}

/// What the journal records about one ticket's review, and nothing the checkout says.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    /// The ticket.
    pub card: CardId,
    /// The card's current sequence.
    pub seq: Seq,
    /// The installed round.
    pub round: u32,
    /// The installed goal.
    pub goal: GoalId,
    /// Who holds the ticket.
    pub claimant: AgentId,
    /// The tree the claimant's recorded hire names.
    pub tree: Tree,
    /// Where the baseline record sits in the journal.
    pub baseline_lsn: Lsn,
    /// The round-start observation.
    pub baseline: BaselinePayload,
    /// The newest post-check sample, once one is recorded.
    pub stamp: Option<StampPayload>,
}

/// The review `card` holds in `world`, or the finite reason it holds none. A rework, a
/// reassignment and a retirement each change what this returns, which is how a cached manifest
/// finds out it is stale.
///
/// # Errors
///
/// `baseline_unavailable` when the ticket never started or its round has no recorded baseline,
/// and `stale_ticket` when the recorded round, goal, claimant or tree no longer agree.
pub fn recorded(
    world: &World,
    view: &CardView,
    ticket: &TicketRow,
) -> Result<Recorded, DiffRefusal> {
    let goal = ticket.goal.ok_or(DiffRefusal::BaselineUnavailable)?;
    let claimant = view.claimed_by.ok_or(DiffRefusal::StaleTicket)?;
    if world.ticket_goal(goal) != Some((view.id, ticket.round)) {
        return Err(DiffRefusal::StaleTicket);
    }
    let review = world
        .ticket_review(goal)
        .ok_or(DiffRefusal::BaselineUnavailable)?;
    let binding = &review.baseline.binding;
    if (binding.card, binding.round, binding.goal, binding.claimant)
        != (view.id, ticket.round, goal, claimant)
    {
        return Err(DiffRefusal::StaleTicket);
    }
    let Some(GitTarget::Read(tree)) = GitTarget::of(world, claimant) else {
        return Err(DiffRefusal::StaleTicket);
    };
    Ok(Recorded {
        card: view.id,
        seq: view.seq,
        round: ticket.round,
        goal,
        claimant,
        tree,
        baseline_lsn: review.baseline_lsn,
        baseline: review.baseline,
        stamp: review.stamp.map(|(_, stamp)| stamp),
    })
}

/// A recorded review turned into the values a read needs, and the binding every reply carries.
#[derive(Clone, Debug, PartialEq)]
struct Subject {
    binding: DiffBinding,
    tree: Tree,
    baseline: BaselineCapture,
    stamp: Option<StampPayload>,
}

/// One manifest the session holds, with the subject it was made for.
struct Stored {
    manifest: CandidateManifest,
    subject: Subject,
}

#[derive(Default)]
struct Session {
    seen: Option<Instant>,
    last: Option<Instant>,
    charged: usize,
    manifest: Option<Arc<Stored>>,
    /// The manifest this session held ran out its lifetime, as opposed to never existing.
    expired: bool,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<[u8; 32], Session>,
}

impl State {
    /// Drop every manifest whose lifetime has ended, remembering that it did.
    fn expire(&mut self, now: Timestamp) {
        for kept in self.sessions.values_mut() {
            if kept
                .manifest
                .as_ref()
                .is_some_and(|stored| now >= stored.manifest.expires_at())
            {
                kept.manifest = None;
                kept.expired = true;
            }
        }
    }

    /// How many sessions other than `session` hold a manifest.
    fn holders_besides(&self, session: [u8; 32]) -> usize {
        self.sessions
            .iter()
            .filter(|(key, kept)| **key != session && kept.manifest.is_some())
            .count()
    }
}

enum Admitted {
    New,
    Held(Arc<Stored>),
}

enum Work {
    Manifest(Box<Subject>),
    Content(Arc<Stored>, Vec<String>),
}

struct Built {
    bytes: Vec<u8>,
    manifest: Option<CandidateManifest>,
}

/// How a request ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// The complete serialized reply, charged to the session already.
    Body(Vec<u8>),
    /// The listener is stopping and no answer is owed.
    Stopping,
    /// The session ended while the read ran.
    Revoked,
    /// The loop could not answer, so the daemon is ending.
    Unavailable,
}

/// The route's reader and limits, shared by every Floor connection.
pub struct DiffReads {
    reader: Arc<DiffReader>,
    slot: Arc<Mutex<()>>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    salt: [u8; 32],
    state: Mutex<State>,
    abandoned: Arc<AtomicUsize>,
    interval: Duration,
    deadline: Duration,
    allowance: usize,
}

impl fmt::Debug for DiffReads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiffReads").finish_non_exhaustive()
    }
}

fn refused(reason: DiffRefusal) -> Reply {
    Reply::Body(
        serde_json::to_vec(&DiffResponse::Refused { reason }).expect("a finite refusal serializes"),
    )
}

impl DiffReads {
    /// The route over `reader`, taking `slot` for every read, stamping manifests with `clock`.
    #[must_use]
    pub fn new(
        reader: Arc<DiffReader>,
        slot: Arc<Mutex<()>>,
        clock: impl Fn() -> Timestamp + Send + Sync + 'static,
    ) -> Self {
        let mut salt = [0_u8; 32];
        getrandom::fill(&mut salt).expect("the operating system supplies randomness");
        Self {
            reader,
            slot,
            clock: Arc::new(clock),
            salt,
            state: Mutex::default(),
            abandoned: Arc::default(),
            interval: SESSION_INTERVAL,
            deadline: crate::git_read::READ_DEADLINE + GRACE,
            allowance: SESSION_ALLOWANCE,
        }
    }

    /// The same route with a smaller session allowance, for tests that would otherwise have to
    /// receive a mebibyte to reach it.
    #[must_use]
    #[doc(hidden)]
    pub const fn with_allowance(mut self, allowance: usize) -> Self {
        self.allowance = allowance;
        self
    }

    /// The same route with other limits, for tests that would otherwise wait out the real ones.
    #[must_use]
    #[doc(hidden)]
    pub const fn with_limits(mut self, interval: Duration, deadline: Duration) -> Self {
        self.interval = interval;
        self.deadline = deadline;
        self
    }

    /// How many abandoned workers have not yet ended.
    #[must_use]
    #[doc(hidden)]
    pub fn abandoned(&self) -> usize {
        self.abandoned.load(Ordering::Acquire)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fingerprint(&self, domain: &[u8], parts: &[&[u8]]) -> String {
        let mut hash = Sha256::new();
        hash.update(self.salt);
        hash.update(domain);
        for part in parts {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part);
        }
        crate::git_read::diff::hex(&hash.finalize())
    }

    fn bind(&self, recorded: &Recorded) -> Result<Subject, DiffRefusal> {
        let BaselineOutcome::Captured {
            device,
            inode,
            head,
            preexisting_tracked,
        } = recorded.baseline.outcome
        else {
            return Err(DiffRefusal::BaselineUnavailable);
        };
        let baseline_bytes =
            serde_json::to_vec(&recorded.baseline).map_err(|_| DiffRefusal::Unreadable)?;
        let tree_text = format!("{:?}", recorded.tree);
        let binding = DiffBinding {
            card: recorded.card,
            seq: recorded.seq,
            round: recorded.round,
            goal: recorded.goal,
            claimant: recorded.claimant,
            check_lsn: recorded.stamp.as_ref().map(|stamp| stamp.check_lsn),
            baseline_id: self.fingerprint(
                b"baseline",
                &[&recorded.baseline_lsn.0.to_le_bytes(), &baseline_bytes],
            ),
            target_id: self.fingerprint(
                b"target",
                &[
                    tree_text.as_bytes(),
                    &device.to_le_bytes(),
                    &inode.to_le_bytes(),
                ],
            ),
            sampler_version: SAMPLER_VERSION,
            policy_version: TEXT_POLICY_VERSION,
            preexisting_tracked,
        };
        Ok(Subject {
            binding,
            tree: recorded.tree.clone(),
            baseline: BaselineCapture {
                root: RootIdentity { device, inode },
                head,
                preexisting_tracked,
            },
            stamp: recorded.stamp,
        })
    }

    /// Admit one request of `session`, or the reason it is refused. An admitted request has
    /// started its interval, and a manifest request has dropped the session's old manifest.
    fn admit(
        &self,
        session: [u8; 32],
        request: &DiffRequest,
        now: Timestamp,
    ) -> Result<Admitted, DiffRefusal> {
        let at = Instant::now();
        let mut state = self.state();
        state
            .sessions
            .retain(|_, kept| kept.seen.is_none_or(|seen| at.duration_since(seen) < KEEP));
        state.expire(now);
        let others = state.holders_besides(session);
        let kept = state.sessions.entry(session).or_default();
        kept.seen = Some(at);
        if kept.charged >= self.allowance {
            return Err(DiffRefusal::SessionLimit);
        }
        if self.abandoned.load(Ordering::Acquire) >= MAX_ABANDONED
            || kept
                .last
                .is_some_and(|last| at.duration_since(last) < self.interval)
        {
            return Err(DiffRefusal::Busy);
        }
        let admitted = match request {
            DiffRequest::Manifest { .. } => {
                if kept.manifest.is_none() && others >= MAX_MANIFESTS {
                    return Err(DiffRefusal::Busy);
                }
                kept.manifest = None;
                kept.expired = false;
                Admitted::New
            }
            DiffRequest::Page { manifest_id, .. } | DiffRequest::Content { manifest_id, .. } => {
                match &kept.manifest {
                    Some(stored) if stored.manifest.id() == manifest_id => {
                        Admitted::Held(Arc::clone(stored))
                    }
                    None if kept.expired => return Err(DiffRefusal::Expired),
                    _ => return Err(DiffRefusal::StaleManifest),
                }
            }
        };
        kept.last = Some(at);
        Ok(admitted)
    }

    fn resolve(
        &self,
        lookup: &impl Fn(CardId) -> Option<Result<Recorded, DiffRefusal>>,
        card: CardId,
    ) -> Option<Result<Subject, DiffRefusal>> {
        lookup(card).map(|found| found.and_then(|recorded| self.bind(&recorded)))
    }

    fn drop_manifest(&self, session: [u8; 32]) {
        if let Some(kept) = self.state().sessions.get_mut(&session) {
            kept.manifest = None;
        }
    }

    /// Answer one authenticated request from `session`.
    ///
    /// `lookup` asks the loop what the journal records about a ticket and answers `None` when the
    /// loop has ended. `stopped` is polled while a read runs, and `live` asks whether the session
    /// still stands once the read is done.
    pub fn serve(
        &self,
        session: [u8; 32],
        request: &DiffRequest,
        lookup: impl Fn(CardId) -> Option<Result<Recorded, DiffRefusal>>,
        stopped: impl Fn() -> bool,
        live: impl Fn() -> bool,
    ) -> Reply {
        if !cfg!(unix) {
            return refused(DiffRefusal::UnsupportedPlatform);
        }
        let now = (self.clock)();
        let admitted = match self.admit(session, request, now) {
            Ok(admitted) => admitted,
            Err(reason) => return refused(reason),
        };
        let card = match (request, &admitted) {
            (DiffRequest::Manifest { card, .. }, _) => *card,
            (_, Admitted::Held(stored)) => stored.subject.binding.card,
            (_, Admitted::New) => return refused(DiffRefusal::InvalidRequest),
        };
        let subject = match self.resolve(&lookup, card) {
            None => return Reply::Unavailable,
            Some(Err(reason)) => {
                self.drop_manifest(session);
                return refused(reason);
            }
            Some(Ok(subject)) => subject,
        };
        let current = match (request, &admitted) {
            (DiffRequest::Manifest { expected_seq, .. }, _) => subject.binding.seq == *expected_seq,
            (_, Admitted::Held(stored)) => stored.subject == subject,
            (_, Admitted::New) => false,
        };
        if !current {
            self.drop_manifest(session);
            return refused(DiffRefusal::StaleTicket);
        }
        let built = match self.build(request, &admitted, &subject, now, &stopped) {
            Ok(Some(built)) => built,
            Ok(None) => return Reply::Stopping,
            Err(reason) => return refused(reason),
        };
        if !live() {
            return Reply::Revoked;
        }
        match self.resolve(&lookup, card) {
            None => return Reply::Unavailable,
            Some(Ok(again)) if again == subject => {}
            Some(_) => {
                self.drop_manifest(session);
                return refused(DiffRefusal::StaleTicket);
            }
        }
        self.deliver(session, built, subject, now)
    }

    /// Charge the whole reply to the session, then keep a new manifest. Nothing is kept or
    /// charged unless the reply fits the session's allowance.
    fn deliver(&self, session: [u8; 32], built: Built, subject: Subject, now: Timestamp) -> Reply {
        let mut state = self.state();
        state.expire(now);
        let others = state.holders_besides(session);
        let Some(kept) = state.sessions.get_mut(&session) else {
            return refused(DiffRefusal::StaleManifest);
        };
        if kept.charged.saturating_add(built.bytes.len()) > self.allowance {
            kept.charged = self.allowance;
            return refused(DiffRefusal::SessionLimit);
        }
        if built.manifest.is_some() && others >= MAX_MANIFESTS {
            return refused(DiffRefusal::Busy);
        }
        kept.charged += built.bytes.len();
        if let Some(manifest) = built.manifest {
            kept.manifest = Some(Arc::new(Stored { manifest, subject }));
        }
        Reply::Body(built.bytes)
    }

    fn build(
        &self,
        request: &DiffRequest,
        admitted: &Admitted,
        subject: &Subject,
        now: Timestamp,
        stopped: &impl Fn() -> bool,
    ) -> Result<Option<Built>, DiffRefusal> {
        let work = match (request, admitted) {
            (DiffRequest::Manifest { .. }, _) => Work::Manifest(Box::new(subject.clone())),
            (DiffRequest::Content { entries, .. }, Admitted::Held(stored)) => {
                Work::Content(Arc::clone(stored), entries.clone())
            }
            (DiffRequest::Page { cursor, .. }, Admitted::Held(stored)) => {
                let _slot = take(&self.slot)?;
                let encoded = stored.manifest.page(Some(cursor))?;
                return Ok(Some(Built {
                    bytes: encoded.bytes().to_vec(),
                    manifest: None,
                }));
            }
            _ => return Err(DiffRefusal::InvalidRequest),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker = Arc::new(Worker::default());
        let finish = Finish {
            worker: Arc::clone(&worker),
            abandoned: Arc::clone(&self.abandoned),
        };
        let reader = Arc::clone(&self.reader);
        let slot = Arc::clone(&self.slot);
        let flag = Arc::clone(&cancelled);
        let spawned = thread::Builder::new()
            .name("dudes-diff-read".to_owned())
            .spawn(move || {
                let _finish = finish;
                let _ = tx.send(run(&reader, &slot, work, now, flag));
            });
        if spawned.is_err() {
            return Err(DiffRefusal::Unreadable);
        }
        let started = Instant::now();
        loop {
            if stopped() {
                cancelled.store(true, Ordering::Release);
                worker.abandon(&self.abandoned);
                return Ok(None);
            }
            let left = self.deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                cancelled.store(true, Ordering::Release);
                worker.abandon(&self.abandoned);
                return Err(DiffRefusal::TimedOut);
            }
            match rx.recv_timeout(left.min(POLL)) {
                Ok(built) => return built.map(Some),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(DiffRefusal::Unreadable),
            }
        }
    }
}

/// The daemon-wide slot, or `busy` while another read or capture holds it.
fn take(slot: &Mutex<()>) -> Result<std::sync::MutexGuard<'_, ()>, DiffRefusal> {
    match slot.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => Err(DiffRefusal::Busy),
    }
}

/// Whether the checkout still matches the newest recorded stamp. Only a complete stamp taken by
/// the sampler now running can be matched, and only a complete current sample can disagree with
/// it; anything else is unknown.
fn freshness(stamp: Option<&StampPayload>, digest: &str) -> DiffFreshness {
    let Some(StampPayload {
        versions,
        outcome:
            StampOutcome::Sampled {
                complete: true,
                digest: recorded,
                ..
            },
        ..
    }) = stamp
    else {
        return DiffFreshness::Unknown;
    };
    if versions.sampler != SAMPLER_VERSION || versions.policy != TEXT_POLICY_VERSION {
        return DiffFreshness::Unknown;
    }
    if recorded.as_str() == digest {
        DiffFreshness::MatchesReviewSample
    } else {
        DiffFreshness::ChangedAfterCheck
    }
}

/// One read, on the worker, holding the daemon-wide slot.
fn run(
    reader: &DiffReader,
    slot: &Mutex<()>,
    work: Work,
    now: Timestamp,
    cancelled: Arc<AtomicBool>,
) -> Result<Built, DiffRefusal> {
    let _slot = take(slot)?;
    match work {
        Work::Manifest(subject) => {
            let capture = reader.capture_review(&subject.tree, &subject.baseline, cancelled)?;
            let fresh = freshness(subject.stamp.as_ref(), &capture.digest);
            let manifest = capture.into_manifest(subject.binding, now, fresh)?;
            let bytes = manifest.page(None)?.bytes().to_vec();
            Ok(Built {
                bytes,
                manifest: Some(manifest),
            })
        }
        Work::Content(stored, entries) => {
            let encoded = reader.read_content(
                &stored.subject.tree,
                &stored.subject.baseline,
                &stored.manifest,
                &entries,
                cancelled,
            )?;
            Ok(Built {
                bytes: encoded.bytes().to_vec(),
                manifest: None,
            })
        }
    }
}
