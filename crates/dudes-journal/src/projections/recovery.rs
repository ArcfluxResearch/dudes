//! Journal-owned recovery reads, certified against a canonical prefix before any absent lookup.

use std::sync::Arc;

use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryBinding;
use dudes_proto::recovery::{RecoveryAuthorized, RecoveryClaimRef, RecoveryPlan, RecoveryWork};
use dudes_proto::recovery_state::admission::{
    RecoveryAgentCurrent, RecoveryCurrent, RecoveryGoalCurrent, RecoveryLeaseCurrent,
};
use dudes_proto::recovery_state::cache::{RecoveryCache, RecoveryCacheReader, RecoveryCacheRow};
use dudes_proto::recovery_state::history::{
    RecoveryAttemptContext, RecoveryHoldWitness, RecoveryPriorControl, RecoveryStandingControl,
    RecoveryWitness, RecoveryWitnessContext, RecoveryWitnessOwner,
};
use dudes_proto::recovery_state::transition::RecoveryTransition;
use dudes_proto::recovery_state::{
    RecoveryAttempt, RecoveryAttemptFacts, RecoveryClaimState, RecoveryEpisode, RecoveryFacts,
    RecoveryStateError,
};
use dudes_proto::{Entry, Record};
use dudes_types::{GoalStatus, Lsn};
use redb::{ReadableDatabase, ReadableTable};

use super::{
    LAST_APPLIED_CRC, LAST_APPLIED_LSN, META, PROJECTION_VERSION, Projections, RECOVERY,
    SCHEMA_VERSION,
};
use crate::error::{Error, RecoveryStoreError, Result};
use crate::reader::ValidatedPrefix;
use crate::writer::{PreparedAppend, Writer};

pub(super) const LAST_RETIREMENT: &str = "recovery_last_retirement_lsn";

mod fold;
pub(super) use fold::prepare_record;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct Certification {
    store: Arc<()>,
    writer: Arc<()>,
    origin: ValidatedPrefix,
    frontier: Lsn,
    cut: Lsn,
    crc: Option<u64>,
    retirement: Option<u64>,
}

/// An owned historical receipt. Reading it never submits or releases work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReceipt {
    /// Exact original claim.
    pub claim: RecoveryClaimRef,
    /// Unchanged request and context intent.
    pub authorization: RecoveryAuthorized,
    /// Committed claim outcome at the checked cut, under the configured sync policy.
    pub state: RecoveryClaimState,
}

/// Detached recovery relationships from one certified journal cut, without live authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckedWorkSubject {
    /// Neither a current episode nor a planned claim names this work.
    NoClaim {
        /// Exact work queried by the caller.
        work: RecoveryWork,
        /// Certified committed prefix.
        through: Lsn,
    },
    /// A current boundary has not been claimed, even if an older plan names this turn.
    UnclaimedBoundary {
        /// Exact work queried by the caller.
        work: RecoveryWork,
        /// Canonical current episode.
        episode: Box<RecoveryEpisode>,
        /// Certified committed prefix.
        through: Lsn,
    },
    /// Independently joined claim and request receipts.
    Claim(Box<CheckedClaimSubject>),
}

/// Immutable receipt and stage-specific current facts, without a session observation or permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedClaimSubject {
    through: Lsn,
    queried_work: RecoveryWork,
    receipt: RecoveryReceipt,
    current_facts: Option<RecoveryFacts>,
}

impl CheckedClaimSubject {
    /// Certified cut shared by the receipt and current facts.
    #[must_use]
    pub const fn through(&self) -> Lsn {
        self.through
    }

    /// Work requested by the caller, which may precede the effective issued work.
    #[must_use]
    pub const fn queried_work(&self) -> RecoveryWork {
        self.queried_work
    }

    /// Original authorization and its durable outcome, without rewriting its binding.
    #[must_use]
    pub const fn receipt(&self) -> &RecoveryReceipt {
        &self.receipt
    }

    /// Current source facts for `Pending`, or exact issued facts for `IssueBound`.
    /// Closed historical claims have no active admission facts.
    #[must_use]
    pub const fn current_facts(&self) -> Option<&RecoveryFacts> {
        self.current_facts.as_ref()
    }
}

/// Actual owned frames committed by one checked append.
#[derive(Debug)]
pub struct CheckedAppend {
    main: (Lsn, Entry),
    checkpoint: Option<(Lsn, Entry)>,
}

impl CheckedAppend {
    /// Position of the caller's entry.
    #[must_use]
    pub const fn lsn(&self) -> Lsn {
        self.main.0
    }

    /// Last physical frame committed and projected by this append.
    #[must_use]
    pub fn through(&self) -> Lsn {
        self.checkpoint.as_ref().map_or(self.main.0, |tail| tail.0)
    }

    /// Consume the caller frame and its optional physical checkpoint.
    #[must_use]
    pub fn into_parts(self) -> ((Lsn, Entry), Option<(Lsn, Entry)>) {
        (self.main, self.checkpoint)
    }
}

/// Which boundary refused a guarded append.
#[derive(Debug)]
pub enum GuardedAppendError<A, F> {
    /// Journal validation, writing or projection failed.
    Journal(Error),
    /// The caller refused the short-lived write permit before any write.
    Acquire(A),
    /// The caller's post-sync boundary failed after this actual caller frame was written.
    AfterFsync {
        /// Actual caller frame position, even when a checkpoint follows it.
        caller: Lsn,
        /// Caller-owned boundary failure.
        source: F,
    },
}

impl<A, F> From<Error> for GuardedAppendError<A, F> {
    fn from(error: Error) -> Self {
        Self::Journal(error)
    }
}

impl<A: std::fmt::Display, F: std::fmt::Display> std::fmt::Display for GuardedAppendError<A, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Journal(error) => error.fmt(f),
            Self::Acquire(error) => write!(f, "append permit refused: {error}"),
            Self::AfterFsync { caller, source } => {
                write!(f, "append boundary after {caller:?}: {source}")
            }
        }
    }
}

impl<A: std::error::Error + 'static, F: std::error::Error + 'static> std::error::Error
    for GuardedAppendError<A, F>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Journal(error) => error,
            Self::Acquire(error) => error,
            Self::AfterFsync { source, .. } => source,
        })
    }
}

struct ProjectionCommit {
    txn: redb::WriteTransaction,
    certification: Certification,
    first: super::AppliedBatch,
}

/// Exclusive access to the actual writer and its canonically checked projection generation.
///
/// Registered recovery records use this checked boundary. Daemon command wiring,
/// authenticated admission and dispatch remain outside this library boundary.
pub struct CheckedRecoveryStore<'a> {
    writer: &'a mut Writer,
    projections: &'a mut Projections,
}

impl<'a> CheckedRecoveryStore<'a> {
    /// Compose recovery subjects at one certified cut without observing live authority.
    pub fn authority_subjects(&self, works: &[RecoveryWork]) -> Result<Vec<CheckedWorkSubject>> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        let tables = FactTables {
            agents: &txn.open_table(super::AGENTS)?,
            goals: &txn.open_table(super::GOALS)?,
            turns: &txn.open_table(super::TURNS)?,
            board: &txn.open_table(super::BOARD)?,
            leases: &txn.open_table(super::WORKTREES)?,
            recovery: &txn.open_table(RECOVERY)?,
            meta: &txn.open_table(META)?,
        };
        works
            .iter()
            .map(|work| subject_at(&tables, self.projections.last_applied, *work))
            .collect()
    }

    /// Bind a target to current rows and exact sources in this owned, certified prefix.
    /// This factual result grants no operator permission and releases no work.
    pub fn facts(&self, binding: &RecoveryBinding) -> Result<RecoveryFacts> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        facts_at(
            &FactTables {
                agents: &txn.open_table(super::AGENTS)?,
                goals: &txn.open_table(super::GOALS)?,
                turns: &txn.open_table(super::TURNS)?,
                board: &txn.open_table(super::BOARD)?,
                leases: &txn.open_table(super::WORKTREES)?,
                recovery: &txn.open_table(RECOVERY)?,
                meta: &txn.open_table(META)?,
            },
            self.projections.last_applied,
            binding,
        )
    }

    /// Validate a typed recovery transition without changing rows or the journal.
    /// This preparation is not authenticated command admission and cannot release work.
    pub fn prepare_transition(&self, transition: RecoveryTransition<'_>) -> Result<()> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        prepare_at(
            &FactTables {
                agents: &txn.open_table(super::AGENTS)?,
                goals: &txn.open_table(super::GOALS)?,
                turns: &txn.open_table(super::TURNS)?,
                board: &txn.open_table(super::BOARD)?,
                leases: &txn.open_table(super::WORKTREES)?,
                recovery: &txn.open_table(RECOVERY)?,
                meta: &txn.open_table(META)?,
            },
            self.projections.last_applied,
            transition,
        )
    }

    /// Read one actual source at the same checked cut, without interpreting permission.
    pub fn witness(&self, lsn: Lsn) -> Result<RecoveryWitness> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        let table = txn.open_table(RECOVERY)?;
        let source = RecoveryCache::load_sources(
            &CacheReader(&table),
            self.projections.last_applied,
            &[lsn],
        )?;
        Ok(source
            .history()
            .at(lsn)
            .map_err(|_| RecoveryStateError::Binding)?
            .clone())
    }

    pub(crate) fn open(writer: &'a mut Writer, projections: &'a mut Projections) -> Result<Self> {
        if let Err(error) = certain(writer) {
            projections.recovery_certification = None;
            return Err(error);
        }
        if projections.recovery_certification.is_some() {
            if let Err(error) = check(writer, projections) {
                projections.recovery_certification = None;
                return Err(error);
            }
        } else {
            let frontier = writer.next_lsn()?;
            let (origin, canonical) = writer.reader().canonical_recovery_projection()?;
            certain(writer)?;
            if frontier != writer.next_lsn()? {
                return refused(RecoveryStoreError::JournalAdvanced);
            }
            if authority_rows(projections)? != authority_rows(&canonical)? {
                return refused(RecoveryStoreError::CanonicalMismatch);
            }
            projections.recovery_certification = Some(Certification {
                store: Arc::clone(&projections.recovery_generation),
                writer: Arc::clone(&writer.recovery_generation),
                origin,
                frontier,
                cut: projections.last_applied,
                crc: projections.last_applied_crc,
                retirement: projections
                    .db
                    .begin_read()?
                    .open_table(META)?
                    .get(LAST_RETIREMENT)?
                    .map(|value| value.value()),
            });
        }
        Ok(Self {
            writer,
            projections,
        })
    }

    /// Look up an original request at a certain journal cut, without appending or retrying it.
    pub fn receipt(&self, request: Hex32, digest: Hex32) -> Result<Option<RecoveryReceipt>> {
        self.read_receipt(request, Some(digest))
    }

    /// Read an original request at a certified cut using only its retained identity.
    pub fn receipt_by_request(&self, request: Hex32) -> Result<Option<RecoveryReceipt>> {
        self.read_receipt(request, None)
    }

    fn read_receipt(
        &self,
        request: Hex32,
        digest: Option<Hex32>,
    ) -> Result<Option<RecoveryReceipt>> {
        check(self.writer, self.projections)?;
        let txn = self.projections.db.begin_read()?;
        check_metadata(self.projections, &txn)?;
        let table = txn.open_table(RECOVERY)?;
        let cache = RecoveryCache::load_receipt(
            &CacheReader(&table),
            self.projections.last_applied,
            request,
        )?;
        let receipt = match digest {
            Some(digest) => cache.state().receipt(request, digest)?,
            None => cache.state().receipt_by_request(request)?,
        };
        Ok(receipt.map(|receipt| RecoveryReceipt {
            claim: receipt.claim,
            authorization: receipt.authorization.clone(),
            state: receipt.state.clone(),
        }))
    }

    /// Validate folds, append that same entry under the configured sync policy, and commit its rows.
    ///
    /// A failure after append may leave the entry recorded. It fences this writer and requires
    /// reopening/reconstruction before another checked receipt can be read. Existing daemon
    /// authority checks remain required when this boundary is wired into the append path.
    pub fn append(&mut self, entry: &Entry) -> Result<Lsn> {
        match self.append_guarded(
            entry,
            || Ok::<_, std::convert::Infallible>(()),
            || Ok::<_, std::convert::Infallible>(()),
        ) {
            Ok(append) => Ok(append.lsn()),
            Err(GuardedAppendError::Journal(error)) => Err(error),
            Err(
                GuardedAppendError::Acquire(never)
                | GuardedAppendError::AfterFsync { source: never, .. },
            ) => match never {},
        }
    }

    /// Preflight the owned entry before acquiring a guard held only during journal commit.
    ///
    /// The guard is dropped before the post-sync callback and projection commit. A failure after
    /// writing fences this writer and yields no acknowledgment; it may leave durable frames.
    pub fn append_guarded<G, A, F>(
        &mut self,
        entry: &Entry,
        acquire: impl FnOnce() -> std::result::Result<G, A>,
        after_fsync: impl FnOnce() -> std::result::Result<(), F>,
    ) -> std::result::Result<CheckedAppend, GuardedAppendError<A, F>> {
        let (prepared, projection) = self.prepare_checked(entry)?;
        let lsn = prepared.lsn();
        let crc = prepared.crc();
        let guard = acquire().map_err(GuardedAppendError::Acquire)?;
        let written = self.writer.commit_prepared(prepared);
        drop(guard);
        let (written_lsn, written_crc) = match written {
            Ok(value) => value,
            Err(error) => {
                if self.writer.poisoned().is_some()
                    || self.writer.next_lsn().ok() != Some(projection.certification.frontier)
                {
                    self.writer.recovery_fenced = true;
                }
                return Err(GuardedAppendError::Journal(error));
            }
        };
        if let Err(error) = certain(self.writer) {
            self.writer.recovery_fenced = true;
            return Err(GuardedAppendError::Journal(error));
        }
        if let Err(source) = after_fsync() {
            self.writer.recovery_fenced = true;
            return Err(GuardedAppendError::AfterFsync {
                caller: written_lsn,
                source,
            });
        }
        let result =
            self.commit_projection(entry, (lsn, crc), (written_lsn, written_crc), projection);
        if result.is_err() {
            self.writer.recovery_fenced = true;
        }
        result.map_err(GuardedAppendError::Journal)
    }

    fn prepare_checked<'e>(
        &mut self,
        entry: &'e Entry,
    ) -> Result<(PreparedAppend<'e>, ProjectionCommit)> {
        check(self.writer, self.projections)?;
        self.projections.validate_next_instruction(entry)?;
        let prepared = self.writer.prepare_append(entry, false)?;
        let mut txn = self.projections.db.begin_write()?;
        txn.set_durability(self.projections.durability)?;
        check_metadata_table(self.projections, &txn.open_table(META)?)?;
        let certification = self
            .projections
            .recovery_certification
            .take()
            .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
        let first = super::fold_batch(
            &txn,
            self.projections.last_applied,
            [(prepared.lsn(), prepared.crc(), prepared.entry())],
        )
        .map_err(|error| preappend_error(entry, error))?;
        if first.applied != 1 {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        Ok((
            prepared,
            ProjectionCommit {
                txn,
                certification,
                first,
            },
        ))
    }

    fn commit_projection(
        &mut self,
        entry: &Entry,
        expected: (Lsn, u32),
        written: (Lsn, u32),
        projection: ProjectionCommit,
    ) -> Result<CheckedAppend> {
        certain(self.writer)?;
        if written != expected {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        let (lsn, crc) = written;
        let ProjectionCommit {
            txn,
            certification,
            first,
        } = projection;
        let frontier = self.writer.next_lsn()?;
        let frames = written_interval(self.writer, lsn, crc, entry, frontier)?;
        let last = if frames.len() == 2 {
            let (at, witness, checkpoint) = &frames[1];
            super::fold_batch(&txn, first.last, [(*at, *witness, checkpoint)])?
        } else {
            first
        };
        let retirement = txn
            .open_table(META)?
            .get(LAST_RETIREMENT)?
            .map(|value| value.value());
        txn.commit()?;
        self.projections.publish_batch(&last);
        self.projections.recovery_certification = Some(Certification {
            store: certification.store,
            writer: certification.writer,
            origin: certification.origin,
            frontier,
            cut: self.projections.last_applied,
            crc: self.projections.last_applied_crc,
            retirement,
        });
        let mut frames = frames.into_iter();
        let (at, _, main) = frames
            .next()
            .ok_or(Error::RecoveryStore(RecoveryStoreError::AppendMismatch))?;
        Ok(CheckedAppend {
            main: (at, main),
            checkpoint: frames.next().map(|(at, _, tail)| (at, tail)),
        })
    }
}

pub(super) struct FactTables<'a, Rows, Sources, Meta> {
    pub agents: &'a Rows,
    pub goals: &'a Rows,
    pub turns: &'a Rows,
    pub board: &'a Rows,
    pub leases: &'a Rows,
    pub recovery: &'a Sources,
    pub meta: &'a Meta,
}

fn subject_at<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    work: RecoveryWork,
) -> Result<CheckedWorkSubject>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let scoped = RecoveryCache::load_work(&CacheReader(tables.recovery), through, work)?;
    let state = scoped.state();
    let claim = match state.work_claim(work) {
        Ok(Some(claim)) => claim,
        Ok(None) => return Ok(CheckedWorkSubject::NoClaim { work, through }),
        Err(RecoveryStateError::Lifecycle)
            if state
                .current(work)
                .is_some_and(|episode| episode.authorization_lsn.is_none()) =>
        {
            return Ok(CheckedWorkSubject::UnclaimedBoundary {
                work,
                episode: Box::new(
                    state
                        .current(work)
                        .ok_or(RecoveryStateError::Binding)?
                        .clone(),
                ),
                through,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let original = state.claim_receipt(claim)?;
    let request = RecoveryCache::load_receipt(
        &CacheReader(tables.recovery),
        through,
        original.authorization.request_id,
    )?;
    if request.state().receipt(
        original.authorization.request_id,
        original.authorization.request_digest,
    )? != Some(original.clone())
    {
        return Err(RecoveryStateError::Binding.into());
    }
    let receipt = RecoveryReceipt {
        claim,
        authorization: original.authorization.clone(),
        state: original.state.clone(),
    };
    let current_facts = claim_facts_at(tables, through, work, &receipt)?;
    Ok(CheckedWorkSubject::Claim(Box::new(CheckedClaimSubject {
        through,
        queried_work: work,
        receipt,
        current_facts,
    })))
}

fn claim_facts_at<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    queried: RecoveryWork,
    receipt: &RecoveryReceipt,
) -> Result<Option<RecoveryFacts>>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let authorization = &receipt.authorization;
    let target = match &authorization.plan {
        RecoveryPlan::New { turn, .. } => RecoveryWork {
            turn: *turn,
            ..authorization.binding.work
        },
        RecoveryPlan::RepeatIssued { issue, .. } => issue.issued.work,
    };
    if queried != authorization.binding.work && queried != target {
        return Err(RecoveryStateError::Binding.into());
    }
    let mut binding = authorization.binding.clone();
    let issued = match &receipt.state {
        RecoveryClaimState::Pending => None,
        RecoveryClaimState::IssueBound(issue) => {
            binding.work = issue.issued.work;
            binding.issue_lsn = issue.lsn;
            Some(issue)
        }
        RecoveryClaimState::Consumed { .. }
        | RecoveryClaimState::Invalidated { .. }
        | RecoveryClaimState::Superseded { .. } => return Ok(None),
    };
    let facts = facts_at(tables, through, &binding)?;
    if issued.is_some_and(|issue| facts.source_issue != *issue) {
        return Err(RecoveryStateError::Binding.into());
    }
    if facts.control_lsn != authorization.resulting_control {
        return Err(RecoveryStateError::Control.into());
    }
    Ok(Some(facts))
}

pub(super) fn facts_at<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    binding: &RecoveryBinding,
) -> Result<RecoveryFacts>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let agent = super::get_row::<super::AgentRow>(tables.agents, &binding.work.agent.to_bytes())?
        .ok_or(RecoveryStateError::Binding)?;
    let goal = super::get_row::<super::GoalRow>(tables.goals, &binding.work.goal.to_bytes())?
        .ok_or(RecoveryStateError::Binding)?;
    let turn = super::get_row::<super::TurnRow>(tables.turns, &binding.work.turn.to_bytes())?
        .ok_or(RecoveryStateError::Binding)?;
    let table = tables.recovery;
    let initial = RecoveryCache::load_sources(
        &CacheReader(table),
        through,
        &[turn.lsn_issued, goal.installed_lsn],
    )?;
    let mut issue = initial
        .history()
        .at(turn.lsn_issued)
        .map_err(|_| RecoveryStateError::Binding)?
        .issue()
        .ok_or(RecoveryStateError::Binding)?;
    issue.issued.work.agent = turn.agent;
    issue.issued.work.goal = turn.goal.ok_or(RecoveryStateError::Binding)?;
    issue
        .issued
        .model_selection
        .clone_from(&turn.model_selection);
    let ticket = current_ticket(tables.board, &goal, initial.history())?;
    let lease = agent
        .worktree
        .map(|id| {
            let row = super::get_row::<super::WorktreeRow>(tables.leases, &id.to_bytes())?
                .ok_or(RecoveryStateError::Binding)?;
            Ok::<_, Error>(RecoveryLeaseCurrent {
                worktree: id,
                agent: row.agent,
                removed: row.removed,
            })
        })
        .transpose()?;
    let current = RecoveryCurrent {
        through,
        agent: current_agent(&agent),
        goal: RecoveryGoalCurrent {
            goal: goal.id,
            agent: goal.agent,
            installed_lsn: goal.installed_lsn,
            last_turn: goal.last_turn,
            cleared: goal.status == GoalStatus::Abandoned,
            done: goal.status == GoalStatus::Achieved,
            predicate: &goal.predicate,
            result_lsn: goal.last_result.map(|result| result.lsn),
            card_change_lsn: match goal.predicate {
                dudes_proto::Predicate::BoardState { card, .. } => {
                    let row =
                        super::get_row::<super::board::BoardRow>(tables.board, &card.to_bytes())?
                            .ok_or(RecoveryStateError::Binding)?;
                    Some(row.recovery_change_lsn)
                }
                _ => None,
            },
            retired_lsn: tables
                .meta
                .get(LAST_RETIREMENT)?
                .map(|value| Lsn(value.value())),
        },
        issue,
        mode: turn.mode,
        attempt: RecoveryAttempt {
            spawn_lsn: turn.spawn_lsn,
            end_lsn: turn.end_lsn,
            confirmation_lsn: turn.confirmation_lsn,
        },
        ticket,
        lease,
    };
    let sources = RecoveryCache::load_sources(&CacheReader(table), through, &current.sources())?;
    Ok(current.bind(binding, sources.history())?)
}

pub(super) fn prepare_at<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    transition: RecoveryTransition<'_>,
) -> Result<()>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
    Meta: ReadableTable<&'static str, u64>,
{
    let scoped =
        RecoveryCache::load_transition(&CacheReader(tables.recovery), through, transition)?;
    if let RecoveryTransition::Invalidated(record) = transition {
        let positions = match record.evidence {
            dudes_proto::recovery::RecoveryInvalidationEvidence::RecordedChange { lsn, .. } => {
                vec![lsn]
            }
            dudes_proto::recovery::RecoveryInvalidationEvidence::BrowserAuthorityUnavailable {
                ..
            } => Vec::new(),
        };
        let sources =
            RecoveryCache::load_sources(&CacheReader(tables.recovery), through, &positions)?;
        return Ok(scoped
            .state()
            .validate_invalidation(record, through, sources.history())?);
    }
    if let RecoveryTransition::Consumed {
        claim,
        issue,
        evidence,
    } = transition
    {
        let facts = attempt_at(tables, through, issue)?;
        return Ok(scoped
            .state()
            .validate_consumed(claim, issue, evidence, &facts)?);
    }
    let binding = transition.binding(scoped.state())?;
    let facts = facts_at(tables, through, &binding)?;
    let sources = RecoveryCache::load_sources(
        &CacheReader(tables.recovery),
        through,
        &transition.sources(&facts),
    )?;
    scoped
        .state()
        .validate_transition(transition, &facts, sources.history())?;
    validate_new_turn(tables.turns, transition)
}

fn validate_new_turn<Rows>(turns: &Rows, transition: RecoveryTransition<'_>) -> Result<()>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
{
    if let RecoveryTransition::Authorized(record) = transition
        && let dudes_proto::recovery::RecoveryPlan::New { turn, .. } = &record.plan
        && turns.get(&turn.to_bytes())?.is_some()
    {
        return Err(RecoveryStateError::Binding.into());
    }
    Ok(())
}

fn attempt_at<Rows, Sources, Meta>(
    tables: &FactTables<'_, Rows, Sources, Meta>,
    through: Lsn,
    issue: &dudes_proto::recovery::RecoveryIssue,
) -> Result<RecoveryAttemptFacts>
where
    Rows: ReadableTable<&'static [u8; 16], &'static [u8]>,
    Sources: ReadableTable<&'static [u8], &'static [u8]>,
{
    let row = super::get_row::<super::TurnRow>(tables.turns, &issue.issued.work.turn.to_bytes())?
        .ok_or(RecoveryStateError::Binding)?;
    let sources =
        RecoveryCache::load_sources(&CacheReader(tables.recovery), through, &[row.lsn_issued])?;
    let recorded = sources
        .history()
        .at(row.lsn_issued)
        .map_err(|_| RecoveryStateError::Binding)?
        .issue()
        .ok_or(RecoveryStateError::Binding)?;
    if row.mode != dudes_types::Mode::Managed
        || row.turn != issue.issued.work.turn
        || row.agent != issue.issued.work.agent
        || row.goal != Some(issue.issued.work.goal)
        || row.model_selection != issue.issued.model_selection
        || recorded != *issue
    {
        return Err(RecoveryStateError::Binding.into());
    }
    Ok(RecoveryAttemptFacts {
        through,
        issue: recorded,
        attempt: RecoveryAttempt {
            spawn_lsn: row.spawn_lsn,
            end_lsn: row.end_lsn,
            confirmation_lsn: row.confirmation_lsn,
        },
    })
}

fn standing_control(agent: &super::AgentRow) -> RecoveryStandingControl {
    RecoveryStandingControl {
        paused: agent.paused,
        halted: agent.halted,
        hold: agent.automatic_hold.as_ref().map(RecoveryHoldWitness::from),
    }
}

fn current_agent(agent: &super::AgentRow) -> RecoveryAgentCurrent {
    RecoveryAgentCurrent {
        agent: agent.id,
        hired_lsn: agent.hired_lsn,
        provider: agent.provider,
        worktree: agent.worktree,
        mode: agent.mode,
        retired: false,
        control: standing_control(agent),
        goal: agent.goal,
        control_lsn: agent.control_lsn,
        session: agent.session.clone(),
        session_lsn: agent.session_lsn,
    }
}

fn current_ticket(
    board: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    goal: &super::GoalRow,
    history: &dudes_proto::recovery_state::history::RecoveryHistory,
) -> Result<Option<dudes_proto::diff_stamp::TicketBinding>> {
    let source = history
        .at(goal.installed_lsn)
        .map_err(|_| RecoveryStateError::Binding)?;
    let RecoveryWitnessOwner::Ticket { card, .. } = source.reference().owner else {
        return Ok(None);
    };
    let row = super::get_row::<super::board::BoardRow>(board, &card.to_bytes())?
        .ok_or(RecoveryStateError::Binding)?;
    let ticket = row.ticket.ok_or(RecoveryStateError::Binding)?;
    if ticket.goal != Some(goal.id) || row.view.claimed_by != Some(goal.agent) {
        return Err(RecoveryStateError::Binding.into());
    }
    Ok(Some(dudes_proto::diff_stamp::TicketBinding {
        card,
        round: ticket.round,
        goal: goal.id,
        claimant: goal.agent,
    }))
}

fn preappend_error(entry: &Entry, error: Error) -> Error {
    match error {
        Error::RecoveryRecord { reason, .. } => Error::RecoveryAdmission {
            kind: entry.record.kind(),
            reason,
        },
        Error::MailWake { reason, .. } => Error::MailWakeAdmission {
            kind: entry.record.kind(),
            reason,
        },
        error => error,
    }
}

fn refused<T>(reason: RecoveryStoreError) -> Result<T> {
    Err(Error::RecoveryStore(reason))
}

fn certain(writer: &Writer) -> Result<()> {
    if writer.poisoned().is_some() || writer.recovery_fenced {
        return refused(RecoveryStoreError::JournalUncertain);
    }
    Ok(())
}

fn check(writer: &Writer, projections: &Projections) -> Result<()> {
    certain(writer)?;
    let certification = projections
        .recovery_certification
        .as_ref()
        .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
    if !Arc::ptr_eq(&certification.store, &projections.recovery_generation)
        || !Arc::ptr_eq(&certification.writer, &writer.recovery_generation)
    {
        return refused(RecoveryStoreError::Uncertified);
    }
    if writer.next_lsn()? != certification.frontier {
        return refused(RecoveryStoreError::JournalAdvanced);
    }
    let txn = projections.db.begin_read()?;
    check_metadata(projections, &txn)
}

fn check_metadata(projections: &Projections, txn: &redb::ReadTransaction) -> Result<()> {
    check_metadata_table(projections, &txn.open_table(META)?)
}

fn check_metadata_table(
    projections: &Projections,
    meta: &impl ReadableTable<&'static str, u64>,
) -> Result<()> {
    let certification = projections
        .recovery_certification
        .as_ref()
        .ok_or(Error::RecoveryStore(RecoveryStoreError::Uncertified))?;
    if meta.get(SCHEMA_VERSION)?.map(|value| value.value()) != Some(PROJECTION_VERSION)
        || meta.get(LAST_APPLIED_LSN)?.map(|value| value.value()) != Some(certification.cut.0)
        || meta.get(LAST_APPLIED_CRC)?.map(|value| value.value()) != certification.crc
        || meta.get(LAST_RETIREMENT)?.map(|value| value.value()) != certification.retirement
        || projections.last_applied != certification.cut
        || projections.last_applied_crc != certification.crc
    {
        return refused(RecoveryStoreError::Metadata);
    }
    Ok(())
}

fn written_interval(
    writer: &Writer,
    lsn: Lsn,
    crc: u32,
    entry: &Entry,
    end: Lsn,
) -> Result<Vec<(Lsn, u32, Entry)>> {
    let mut frames = Vec::with_capacity(2);
    for frame in writer.reader().witnessed_between(lsn, end)? {
        let frame = frame?;
        if frame.0 >= end {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        if frames.len() == 2 {
            return refused(RecoveryStoreError::AppendMismatch);
        }
        frames.push(frame);
    }
    if frames
        .first()
        .is_none_or(|(at, witness, recorded)| *at != lsn || *witness != crc || recorded != entry)
        || frames
            .get(1)
            .is_some_and(|(_, _, recorded)| !matches!(recorded.record, Record::Checkpoint { .. }))
    {
        return refused(RecoveryStoreError::AppendMismatch);
    }
    Ok(frames)
}

struct CacheReader<'a, T>(&'a T);

impl<T: ReadableTable<&'static [u8], &'static [u8]>> RecoveryCacheReader for CacheReader<'_, T> {
    type Error = Error;
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|value| value.value().to_vec()))
    }
    fn prefix(&self, prefix: &[u8]) -> Result<Vec<RecoveryCacheRow>> {
        let mut rows = Vec::new();
        for row in self.0.range(prefix..)? {
            let (key, value) = row?;
            if !key.value().starts_with(prefix) {
                break;
            }
            rows.push((key.value().to_vec(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

pub(super) fn observe(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    through: Lsn,
    lsn: Lsn,
    entry: &Entry,
    context: RecoveryWitnessContext,
) -> Result<()> {
    let mut cache = RecoveryCache::load_history(&CacheReader(table), through, lsn)?;
    cache
        .history_mut()
        .observe_with_context(lsn, entry, context, super::instructions::hash)
        .map_err(dudes_proto::recovery_state::cache::RecoveryCacheError::from)?;
    for update in cache.updates()? {
        match update.value {
            Some(value) => {
                table.insert(update.key.as_slice(), value.as_slice())?;
            }
            None => {
                table.remove(update.key.as_slice())?;
            }
        }
    }
    Ok(())
}

pub(super) fn write_updates(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    updates: Vec<dudes_proto::recovery_state::cache::RecoveryCacheUpdate>,
) -> Result<()> {
    for update in updates {
        match update.value {
            Some(value) => {
                table.insert(update.key.as_slice(), value.as_slice())?;
            }
            None => {
                table.remove(update.key.as_slice())?;
            }
        }
    }
    Ok(())
}

pub(super) fn capture_before(
    agents: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    turns: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    goals: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    entry: &Entry,
) -> Result<RecoveryWitnessContext> {
    let agent = match &entry.record {
        Record::Pause { agent, .. }
        | Record::Resume { agent, .. }
        | Record::Halt { agent, .. }
        | Record::AgentHeld { agent, .. } => Some(*agent),
        _ => None,
    };
    let previous_control = agent
        .map(|agent| {
            Ok::<_, Error>(
                super::get_row::<super::AgentRow>(agents, &agent.to_bytes())?.map(|row| {
                    RecoveryPriorControl {
                        agent,
                        lsn: row.control_lsn,
                        standing: standing_control(&row),
                    }
                }),
            )
        })
        .transpose()?
        .flatten();
    let attempt = match &entry.record {
        Record::TurnSpawned { agent, turn, .. }
        | Record::SessionLearned { agent, turn, .. }
        | Record::TurnEnded { agent, turn, .. } => {
            let row = super::get_row::<super::TurnRow>(turns, &turn.to_bytes())?;
            let owner = super::get_row::<super::AgentRow>(agents, &agent.to_bytes())?;
            row.filter(|row| row.agent == *agent)
                .map(|row| RecoveryAttemptContext {
                    issue_lsn: row.lsn_issued,
                    previous_spawn_lsn: row.spawn_lsn,
                    previous_end_lsn: row.end_lsn,
                    confirmed_session_lsn: owner.and_then(|owner| owner.session_lsn),
                })
        }
        _ => None,
    };
    let qualified_completion = match &entry.record {
        Record::PredicateResult { goal, .. } => {
            super::get_row::<super::GoalRow>(goals, &goal.to_bytes())?
                .filter(|row| !matches!(row.status, GoalStatus::Abandoned | GoalStatus::Achieved))
                .and_then(|row| {
                    dudes_proto::recovery_state::history::qualified_completion(
                        row.agent,
                        row.id,
                        &row.predicate,
                        &entry.record,
                    )
                })
        }
        _ => None,
    };
    Ok(RecoveryWitnessContext {
        qualified_completion,
        attempt,
        previous_control,
        cleared_goal: None,
    })
}

fn authority_rows(projections: &Projections) -> Result<Vec<String>> {
    let txn = projections.db.begin_read()?;
    let meta = txn.open_table(META)?;
    let schema = meta.get(SCHEMA_VERSION)?.map(|value| value.value());
    let cut = meta.get(LAST_APPLIED_LSN)?.map(|value| value.value());
    let crc = meta.get(LAST_APPLIED_CRC)?.map(|value| value.value());
    if schema != Some(PROJECTION_VERSION)
        || cut != Some(projections.last_applied.0)
        || crc != projections.last_applied_crc
    {
        return refused(RecoveryStoreError::Metadata);
    }
    super::instructions::read(
        &txn.open_table(super::INSTRUCTIONS)?,
        projections.last_applied,
    )?;
    RecoveryCache::read_all(
        &CacheReader(&txn.open_table(RECOVERY)?),
        projections.last_applied,
    )?;
    let mut rows = vec![
        format!("schema {PROJECTION_VERSION}"),
        format!(
            "recovery_last_retirement {:?}",
            meta.get(LAST_RETIREMENT)?.map(|value| value.value())
        ),
        projections.export_at(&txn)?,
    ];
    super::export_rows::<16, super::board::BoardRow>(
        &mut rows,
        "board_authority",
        &txn.open_table(super::BOARD)?,
    )?;
    Ok(rows)
}

pub(super) fn export_cells(
    rows: &mut Vec<String>,
    name: &str,
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<()> {
    for row in table.iter()? {
        let (key, value) = row?;
        let value: serde_json::Value =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&value).map_err(|source| Error::Row { source })?;
        rows.push(format!("{name} {} {json}", super::hex(key.value())));
    }
    Ok(())
}
