//! The Floor's opt-in ticket diff route: the closed request, the binding to what the journal
//! recorded, and every limit that keeps one operator session from reading a checkout without end.
//!
//! The route is a pull: the Floor asks when the operator opens or refreshes the review. A
//! **manifest** request resolves the ticket the browser names, reads the checkout once, caches an
//! immutable list of changed files under server-minted ids, and answers its first page. A **page**
//! request continues that list. A **content** request returns up to eight complete files from it.
//! The browser never supplies a path, a revision or a pattern.
//!
//! Three limits are per session and survive reloads and reconnects, because the key
//! is the session cookie's digest. A session may start a read once per interval. It may receive
//! [`SESSION_ALLOWANCE`] bytes of successful replies in all, charged before the reply is sent and
//! never refunded. A reply that does not fit ends the allowance, so a smaller one cannot follow. It may hold one manifest, and the daemon holds at most [`MAX_MANIFESTS`].
//! Manifest and content reads share one daemon-wide slot with the baseline and stamp captures, and
//! give way to a waiting capture. A page reads only the cached manifest and takes no slot.
//!
//! What the journal recorded decides everything the browser is told about freshness. A reply is
//! `unknown` unless the newest recorded stamp is complete, was taken by the sampler now running,
//! and the checkout still hashes to its digest.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use dudes_proto::diff_stamp::{BaselineOutcome, BaselinePayload, StampOutcome, StampPayload};
use dudes_proto::repository::{
    HireRepositories, OwnedRepository, RepoId, RepositoryEntry, RepositoryTurnGrant,
    TicketRepositoryRound,
};
use dudes_proto::repository_review::{RepositoryReview, RepositoryStampOutcome};
use dudes_proto::{
    CardView, DiffBinding, DiffFreshness, DiffRefusal, DiffRepositoryBinding, DiffResponse,
    MAX_DIFF_FILES, RepositoryReadSelection, TicketRow,
};
use dudes_supervisor::World;
use dudes_types::{AgentId, CardId, GoalId, Lsn, Seq, Timestamp};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::diff_capture::ReadSlot;
use crate::floor_git::{Finish, GitTarget, Worker};
use crate::git_read::Tree;
use crate::git_read::diff::{
    BaselineCapture, CandidateManifest, DiffReader, REPOSITORY_SAMPLER_VERSION, ReviewCapture,
    ReviewSubject, RootIdentity, SAMPLER_VERSION, TEXT_POLICY_VERSION,
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

/// The `diff` sibling a Floor snapshot carries exactly when the diff route exists: an empty
/// object that names no path, id or repository, outside `FloorProjection` and `floor.delta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DiffSwitch {}

/// The one value [`DiffSwitch`] takes.
pub const DIFF_SWITCH: DiffSwitch = DiffSwitch {};

/// One bounded request selecting recorded repository ids or server-minted opaque tokens.
#[derive(Clone, PartialEq, Eq)]
pub enum DiffRequest {
    /// Start a review of `card`, which must still be at `expected_seq`.
    Manifest {
        /// The ticket.
        card: CardId,
        /// The card sequence the browser last saw.
        expected_seq: Seq,
        /// Explicit recorded repository scope; omission admits only a singleton.
        repositories: Option<RepositoryReadSelection>,
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
        #[serde(default)]
        repositories: Option<RepositoryReadSelection>,
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
            Wire::Manifest {
                card,
                expected_seq,
                repositories,
            } => {
                if matches!(repositories, Some(RepositoryReadSelection::Hired)) {
                    bail!("ticket review requires a declared repository selection");
                }
                Self::Manifest {
                    card,
                    expected_seq,
                    repositories,
                }
            }
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

/// What the journal records about one expected repository of a ticket round.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RecordedRepository {
    /// Stable identity, absent only for a historical singleton.
    pub repo_id: Option<RepoId>,
    /// Exact recorded execution copy.
    pub tree: Tree,
    /// Recorded baseline, absent when the expected member has no baseline yet.
    pub baseline: Option<BaselinePayload>,
    /// Newest qualifying post-check sample.
    pub stamp: Option<StampPayload>,
}

/// One ticket's immutable recorded review authority, independent of current enrollment.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Recorded {
    /// Ticket identity.
    pub card: CardId,
    /// Current card sequence.
    pub seq: Seq,
    /// Installed round.
    pub round: u32,
    /// Installed goal.
    pub goal: GoalId,
    /// Recorded claimant.
    pub claimant: AgentId,
    /// Exact start, hire, declaration and command-check binding.
    pub start: Option<TicketRepositoryRound>,
    /// Exact scoped hire, absent for a historical singleton.
    pub hired_lsn: Option<Lsn>,
    /// Frozen command-check target.
    pub check_repo: Option<RepoId>,
    /// Baseline record position, absent when a scoped batch has not been captured.
    pub baseline_lsn: Option<Lsn>,
    /// Complete recorded evidence, retained in fingerprinting even when a member cannot be shown.
    pub evidence: Vec<u8>,
    /// Every expected member, in the recorded declaration order.
    pub repositories: Vec<RecordedRepository>,
}

/// The review `card` holds in `world`, or the finite reason it holds none. A rework, a
/// reassignment and a retirement each change what this returns, which is how a cached manifest
/// finds out it is stale.
///
/// # Errors
///
/// `baseline_unavailable` when the ticket never started or its round has no recorded baseline,
/// `stale_ticket` when the recorded round, goal or claimant no longer agree or the journal never
/// hired the claimant, and `worktree_mismatch` when the claimant is retired or its copy removed.
pub fn recorded(
    world: &World,
    view: &CardView,
    ticket: &TicketRow,
) -> Result<Recorded, DiffRefusal> {
    let goal = ticket.goal.ok_or(DiffRefusal::BaselineUnavailable)?;
    if world.ticket_goal(goal) != Some((view.id, ticket.round)) {
        return Err(DiffRefusal::StaleTicket);
    }
    let start = world
        .ticket_repository_round(goal)
        .ok_or(DiffRefusal::StaleTicket)?;
    if let Some(review) = world.repository_review(goal) {
        return scoped_recorded(world, view, ticket, goal, start, review);
    }
    let review = world
        .ticket_review(goal)
        .ok_or(DiffRefusal::BaselineUnavailable)?;
    let binding = &review.baseline.binding;
    let claimant = match view.claimed_by {
        Some(agent) => agent,
        None if retired(world, binding.claimant) => binding.claimant,
        None => return Err(DiffRefusal::StaleTicket),
    };
    if (binding.card, binding.round, binding.goal, binding.claimant)
        != (view.id, ticket.round, goal, claimant)
    {
        return Err(DiffRefusal::StaleTicket);
    }
    Ok(Recorded {
        card: view.id,
        seq: view.seq,
        round: ticket.round,
        goal,
        claimant,
        start: Some(start.clone()),
        hired_lsn: None,
        check_repo: None,
        baseline_lsn: Some(review.baseline_lsn),
        evidence: serde_json::to_vec(review).map_err(|_| DiffRefusal::Unreadable)?,
        repositories: vec![RecordedRepository {
            repo_id: None,
            tree: readable_tree(world, claimant)?,
            baseline: Some(review.baseline),
            stamp: review.stamp.map(|(_, stamp)| stamp),
        }],
    })
}

fn scoped_recorded(
    world: &World,
    view: &CardView,
    ticket: &TicketRow,
    goal: GoalId,
    start: &TicketRepositoryRound,
    review: &RepositoryReview,
) -> Result<Recorded, DiffRefusal> {
    let claimant = review.binding.ticket.claimant;
    if view.claimed_by != Some(claimant) && !(view.claimed_by.is_none() && retired(world, claimant))
    {
        return Err(DiffRefusal::StaleTicket);
    }
    let row = world
        .agents
        .get(&claimant)
        .ok_or(DiffRefusal::StaleTicket)?;
    if row.retired {
        return Err(DiffRefusal::WorktreeMismatch);
    }
    let HireRepositories::Scoped(hire) = &row.repositories else {
        return Err(DiffRefusal::StaleTicket);
    };
    let grant = RepositoryTurnGrant::select(row.mcp.hire_lsn, hire, Some(start))
        .map_err(|_| DiffRefusal::StaleTicket)?;
    if review.binding.start_lsn != start.start_lsn
        || review.binding.hired_lsn != row.mcp.hire_lsn
        || review.binding.ticket.card != view.id
        || review.binding.ticket.goal != goal
        || review.binding.ticket.round != ticket.round
        || !review.expected.iter().eq(grant
            .repositories
            .entries()
            .iter()
            .map(|entry| &entry.repo_id))
    {
        return Err(DiffRefusal::StaleTicket);
    }
    let authority = start.authority.as_ref().ok_or(DiffRefusal::StaleTicket)?;
    let check_copy = grant
        .repositories
        .get(&authority.check_repo)
        .ok_or(DiffRefusal::StaleTicket)?;
    if check_copy.execution != authority.check_root {
        return Err(DiffRefusal::StaleTicket);
    }
    let repositories = grant
        .repositories
        .entries()
        .iter()
        .map(|entry| recorded_member(world, entry, review))
        .collect::<Result<Vec<_>, DiffRefusal>>()?;
    Ok(Recorded {
        card: view.id,
        seq: view.seq,
        round: ticket.round,
        goal,
        claimant,
        start: Some(start.clone()),
        hired_lsn: Some(row.mcp.hire_lsn),
        check_repo: Some(authority.check_repo.clone()),
        baseline_lsn: review.baseline.as_ref().map(|(lsn, _)| *lsn),
        evidence: serde_json::to_vec(review).map_err(|_| DiffRefusal::Unreadable)?,
        repositories,
    })
}

fn recorded_member(
    world: &World,
    entry: &RepositoryEntry<OwnedRepository>,
    review: &RepositoryReview,
) -> Result<RecordedRepository, DiffRefusal> {
    let copy = &entry.value;
    let tree = match world.worktrees.get(&copy.worktree) {
        Some(lease)
            if lease.removed.is_none()
                && lease.path == copy.execution.directory
                && lease.origin == copy.origin.directory
                && lease.base == copy.base =>
        {
            Tree::Recorded(Box::new(copy.clone()))
        }
        _ => return Err(DiffRefusal::WorktreeMismatch),
    };
    let baseline = review
        .baseline
        .as_ref()
        .and_then(|(_, batch)| {
            batch
                .members
                .iter()
                .find(|member| member.repo_id == entry.repo_id)
        })
        .map(|member| BaselinePayload {
            v: 1,
            binding: review.binding.ticket,
            interval: member.interval,
            versions: member.versions,
            outcome: member.outcome,
        });
    let stamp = review.stamp.as_ref().and_then(|(_, batch)| {
        batch
            .members
            .iter()
            .find(|member| member.repo_id == entry.repo_id)
            .map(|member| {
                let outcome = match member.outcome {
                    RepositoryStampOutcome::Sampled {
                        root,
                        complete,
                        digest,
                        omitted,
                        omitted_lower_bound,
                        untracked,
                        untracked_lower_bound,
                    } if root == copy.execution.directory_identity => StampOutcome::Sampled {
                        complete,
                        digest,
                        omitted,
                        omitted_lower_bound,
                        untracked,
                        untracked_lower_bound,
                    },
                    RepositoryStampOutcome::Unavailable { reason } => {
                        StampOutcome::Unavailable { reason }
                    }
                    RepositoryStampOutcome::Sampled { .. } => StampOutcome::Unavailable {
                        reason: DiffRefusal::RootMismatch,
                    },
                };
                StampPayload {
                    v: 1,
                    binding: batch.binding.ticket,
                    baseline_lsn: batch.baseline_lsn,
                    check_lsn: batch.check_lsn,
                    interval: member.interval,
                    versions: member.versions,
                    outcome,
                }
            })
    });
    Ok(RecordedRepository {
        repo_id: Some(entry.repo_id.clone()),
        tree,
        baseline,
        stamp,
    })
}

/// The tree to read for a claimant. A retired agent's tree is not read, whether it was the project
/// folder or a copy of its own, and neither is a removed copy: `worktree_mismatch` says so for
/// each. Only an agent the journal never hired leaves the ticket moved on.
fn readable_tree(world: &World, claimant: AgentId) -> Result<Tree, DiffRefusal> {
    match GitTarget::of(world, claimant) {
        Some(GitTarget::Read(tree)) => Ok(tree),
        Some(GitTarget::Released | GitTarget::Removed) => Err(DiffRefusal::WorktreeMismatch),
        Some(GitTarget::Refused(_) | GitTarget::Repositories { .. }) => {
            Err(DiffRefusal::StaleTicket)
        }
        None if retired(world, claimant) => Err(DiffRefusal::WorktreeMismatch),
        None => Err(DiffRefusal::StaleTicket),
    }
}

fn retired(world: &World, agent: AgentId) -> bool {
    world.agents.get(&agent).is_some_and(|row| row.retired)
}

/// A recorded review turned into the values a read needs, and the binding every reply carries.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct Subject {
    binding: DiffBinding,
    repositories: Vec<SubjectRepository>,
    selection: Option<RepositoryReadSelection>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct SubjectRepository {
    repo_id: Option<RepoId>,
    tree: Tree,
    baseline: Option<BaselineCapture>,
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
    slot: Arc<ReadSlot>,
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
        slot: Arc<ReadSlot>,
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

    fn bind(
        &self,
        recorded: &Recorded,
        selection: Option<&RepositoryReadSelection>,
    ) -> Result<Subject, DiffRefusal> {
        let selected = match selection {
            None if recorded.repositories.len() == 1 => {
                recorded.repositories.iter().collect::<Vec<_>>()
            }
            Some(RepositoryReadSelection::Declared) => recorded.repositories.iter().collect(),
            Some(RepositoryReadSelection::Repository { repo_id }) => recorded
                .repositories
                .iter()
                .filter(|member| member.repo_id.as_ref() == Some(repo_id))
                .collect(),
            _ => return Err(DiffRefusal::InvalidRequest),
        };
        if selected.is_empty() || selected.len() > 8 {
            return Err(DiffRefusal::InvalidRequest);
        }
        let authority = serde_json::to_vec(&(
            recorded.start.as_ref(),
            recorded.hired_lsn,
            &recorded.evidence,
        ))
        .map_err(|_| DiffRefusal::Unreadable)?;
        let mut bindings = Vec::new();
        let mut repositories = Vec::new();
        for member in selected {
            let (binding, repository) =
                self.bind_repository(member, recorded.baseline_lsn, &authority)?;
            bindings.push(binding);
            repositories.push(repository);
        }
        let member_bytes = serde_json::to_vec(&bindings).map_err(|_| DiffRefusal::Unreadable)?;
        let binding = DiffBinding {
            card: recorded.card,
            seq: recorded.seq,
            round: recorded.round,
            goal: recorded.goal,
            claimant: recorded.claimant,
            hired_lsn: recorded.hired_lsn,
            start_lsn: recorded.start.as_ref().map(|start| start.start_lsn),
            check_repo: recorded.check_repo.clone(),
            check_lsn: repositories
                .first()
                .and_then(|member| member.stamp.as_ref().map(|stamp| stamp.check_lsn))
                .filter(|lsn| {
                    repositories.iter().all(|member| {
                        member
                            .stamp
                            .as_ref()
                            .is_some_and(|stamp| stamp.check_lsn == *lsn)
                    })
                }),
            baseline_id: self.fingerprint(b"baseline-set", &[&authority, &member_bytes]),
            target_id: self.fingerprint(b"target-set", &[&authority, &member_bytes]),
            sampler_version: if recorded.hired_lsn.is_some() {
                REPOSITORY_SAMPLER_VERSION
            } else {
                SAMPLER_VERSION
            },
            policy_version: TEXT_POLICY_VERSION,
            preexisting_tracked: bindings.iter().any(|member| member.preexisting_tracked),
            preexisting_untracked: bindings.iter().any(|member| member.preexisting_untracked),
            repositories: bindings,
        };
        Ok(Subject {
            binding,
            repositories,
            selection: selection.cloned(),
        })
    }

    fn bind_repository(
        &self,
        member: &RecordedRepository,
        baseline_lsn: Option<Lsn>,
        authority: &[u8],
    ) -> Result<(DiffRepositoryBinding, SubjectRepository), DiffRefusal> {
        let baseline = member.baseline.and_then(|payload| match payload.outcome {
            BaselineOutcome::Captured {
                device,
                inode,
                head,
                preexisting_tracked,
                preexisting_untracked,
            } => Some(BaselineCapture {
                root: RootIdentity { device, inode },
                head,
                preexisting_tracked,
                preexisting_untracked,
            }),
            _ => None,
        });
        let unavailable = if baseline.is_some() {
            None
        } else {
            Some(match member.baseline.map(|payload| payload.outcome) {
                Some(BaselineOutcome::Unavailable {
                    reason:
                        reason @ (DiffRefusal::UnsupportedStorage | DiffRefusal::UnsupportedPlatform),
                }) => reason,
                Some(BaselineOutcome::Unavailable { reason }) if member.repo_id.is_some() => reason,
                _ => DiffRefusal::BaselineUnavailable,
            })
        };
        if member.repo_id.is_none() && baseline.is_none() {
            return Err(unavailable.unwrap_or(DiffRefusal::BaselineUnavailable));
        }
        let baseline_bytes = serde_json::to_vec(&(baseline_lsn, &member.repo_id, &member.baseline))
            .map_err(|_| DiffRefusal::Unreadable)?;
        let tree_bytes = serde_json::to_vec(&member.tree).map_err(|_| DiffRefusal::Unreadable)?;
        let binding = DiffRepositoryBinding {
            repo_id: member.repo_id.clone(),
            baseline_id: self.fingerprint(b"repository-baseline", &[authority, &baseline_bytes]),
            target_id: self.fingerprint(
                b"repository-target",
                &[authority, &tree_bytes, &baseline_bytes],
            ),
            unavailable,
            preexisting_tracked: baseline
                .as_ref()
                .is_some_and(|base| base.preexisting_tracked),
            preexisting_untracked: baseline
                .as_ref()
                .is_some_and(|base| base.preexisting_untracked),
        };
        let repository = SubjectRepository {
            repo_id: member.repo_id.clone(),
            tree: member.tree.clone(),
            baseline,
            stamp: member.stamp,
        };

        Ok((binding, repository))
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
        selection: Option<&RepositoryReadSelection>,
    ) -> Option<Result<Subject, DiffRefusal>> {
        lookup(card).map(|found| found.and_then(|recorded| self.bind(&recorded, selection)))
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
        let selection = match (request, &admitted) {
            (DiffRequest::Manifest { repositories, .. }, _) => repositories.as_ref(),
            (_, Admitted::Held(stored)) => stored.subject.selection.as_ref(),
            _ => None,
        };
        let subject = match self.resolve(&lookup, card, selection) {
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
        match self.resolve(&lookup, card, selection) {
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

/// Whether the checkout still matches the newest recorded stamp. Only a complete stamp taken by
/// the sampler now running can be matched, and only a complete current sample can disagree with
/// it; anything else is unknown.
fn freshness(stamp: Option<&StampPayload>, current: &ReviewCapture, sampler: u32) -> DiffFreshness {
    if !current.complete {
        return DiffFreshness::Unknown;
    }
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
    if versions.sampler != sampler || versions.policy != TEXT_POLICY_VERSION {
        return DiffFreshness::Unknown;
    }
    if recorded.as_str() == current.digest {
        DiffFreshness::MatchesReviewSample
    } else {
        DiffFreshness::ChangedAfterCheck
    }
}

/// One read, on the worker, holding the daemon-wide slot.
fn run(
    reader: &DiffReader,
    slot: &ReadSlot,
    work: Work,
    now: Timestamp,
    cancelled: Arc<AtomicBool>,
) -> Result<Built, DiffRefusal> {
    let _slot = slot.floor()?;
    match work {
        Work::Manifest(mut subject) => {
            let subjects = read_subjects(&subject);
            let subject_bytes = serde_json::to_vec(&subject)
                .map_err(|_| DiffRefusal::Unreadable)?
                .len();
            let capture = reader.capture_review_batch(&subjects, subject_bytes, cancelled);
            if subject.binding.hired_lsn.is_none()
                && let Some((_, Err(reason))) = capture.members.first()
            {
                return Err(*reason);
            }
            let mut fresh = DiffFreshness::MatchesReviewSample;
            if subjects.len() != subject.repositories.len() {
                fresh = DiffFreshness::Unknown;
            }
            for (repo_id, outcome) in &capture.members {
                if let Err(reason) = outcome
                    && let Some(binding) = subject
                        .binding
                        .repositories
                        .iter_mut()
                        .find(|member| member.repo_id == *repo_id)
                {
                    binding.unavailable = Some(*reason);
                }
                let member = subject
                    .repositories
                    .iter()
                    .find(|member| member.repo_id == *repo_id)
                    .ok_or(DiffRefusal::Unreadable)?;
                let value = outcome.as_ref().map_or(DiffFreshness::Unknown, |capture| {
                    freshness(
                        member.stamp.as_ref(),
                        capture,
                        subject.binding.sampler_version,
                    )
                });
                fresh = match (fresh, value) {
                    (DiffFreshness::Unknown, _) | (_, DiffFreshness::Unknown) => {
                        DiffFreshness::Unknown
                    }
                    (DiffFreshness::ChangedAfterCheck, _)
                    | (_, DiffFreshness::ChangedAfterCheck) => DiffFreshness::ChangedAfterCheck,
                    _ => DiffFreshness::MatchesReviewSample,
                };
            }
            let manifest = capture.into_manifest(subject.binding, now, fresh)?;
            let bytes = manifest.page(None)?.bytes().to_vec();
            Ok(Built {
                bytes,
                manifest: Some(manifest),
            })
        }
        Work::Content(stored, entries) => {
            let encoded = reader.read_content_batch(
                &read_subjects(&stored.subject),
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

fn read_subjects(subject: &Subject) -> Vec<ReviewSubject> {
    subject
        .repositories
        .iter()
        .filter_map(|member| {
            member.baseline.as_ref().map(|baseline| ReviewSubject {
                repo_id: member.repo_id.clone(),
                tree: member.tree.clone(),
                baseline: baseline.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dudes_proto::record::{Actor, samples};
    use dudes_proto::{Entry, Record, RecordKind, TicketCheck};
    use dudes_supervisor::testing::unavailable_baseline;
    use dudes_types::CardStatus;

    use super::*;

    fn world(records: &[Record]) -> World {
        let mut world = World::default();
        for (at, record) in (1..).zip(records) {
            world.apply(
                Lsn(at),
                &Entry {
                    at: Timestamp::from_unix_millis(i64::try_from(at).unwrap()),
                    record: record.clone(),
                },
            );
        }
        world
    }

    #[test]
    fn a_retired_agents_ticket_says_its_copy_is_gone_and_an_unknown_one_has_moved_on() {
        let started = samples::record(RecordKind::TicketStarted);
        let Record::TicketStarted {
            card, agent, goal, ..
        } = started.clone()
        else {
            unreachable!();
        };
        let baseline = Record::TicketBaselineRecorded {
            card,
            goal: goal.id,
            baseline: unavailable_baseline(agent, card, 1, goal.id),
        };
        let plain = samples::record(RecordKind::AgentHired);
        let lease = samples::record(RecordKind::WorktreeLeased);
        let Record::WorktreeLeased { worktree: id, .. } = &lease else {
            unreachable!();
        };
        let mut leased = plain.clone();
        if let Record::AgentHired { worktree, .. } = &mut leased {
            *worktree = Some(*id);
        }
        let retired = samples::record(RecordKind::AgentRetired);
        let view = CardView {
            id: card,
            title: "Ship it".into(),
            department: "engineering".into(),
            status: CardStatus::Review,
            status_by: Actor::System,
            seq: Seq(3),
            claimed_by: Some(agent),
            deps: Vec::new(),
            ticket: None,
        };
        let ticket = TicketRow {
            description: String::new(),
            repositories: "service".into(),
            done_when: TicketCheck {
                id: "default".into(),
                label: "passes".into(),
                predicate: goal.predicate.clone(),
            },
            priority: dudes_proto::Priority::Unset,
            goal: Some(goal.id),
            queue: None,
            message: None,
            round: 1,
            reason: None,
            summary: None,
            last_turn_end: None,
            check: None,
            instruction_review: None,
            coverage: None,
        };
        // What the board holds once the agent retires: a Done ticket's claim is released.
        let done = CardView {
            status: CardStatus::Done,
            seq: Seq(4),
            claimed_by: None,
            ..view.clone()
        };
        let read = |records: &[Record], view: &CardView| {
            recorded(&world(records), view, &ticket)
                .map(|recorded| recorded.repositories[0].tree.clone())
        };

        assert_eq!(
            read(&[plain.clone(), started.clone(), baseline.clone()], &view),
            Ok(Tree::Checkout(PathBuf::from("/work/repo")))
        );
        for (hire, copy) in [(&plain, "the project folder"), (&leased, "its own copy")] {
            let records = [
                lease.clone(),
                hire.clone(),
                started.clone(),
                baseline.clone(),
                retired.clone(),
            ];
            for (shown, status) in [(&view, "ready for review"), (&done, "done")] {
                assert_eq!(
                    read(&records, shown),
                    Err(DiffRefusal::WorktreeMismatch),
                    "a {status} ticket whose agent retired from {copy}"
                );
            }
        }
        assert_eq!(
            read(&[plain, started.clone(), baseline.clone()], &done),
            Err(DiffRefusal::StaleTicket),
            "a released ticket whose agent still works"
        );
        assert_eq!(
            read(&[started, baseline], &view),
            Err(DiffRefusal::StaleTicket),
            "an agent the journal never hired"
        );
    }
}
