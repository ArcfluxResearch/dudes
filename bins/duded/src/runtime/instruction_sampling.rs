use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Instant;

use dudes_proto::instruction::{
    InstructionBootIdentity, InstructionCoverageGap, InstructionGapKind,
    InstructionObservationOwner, InstructionParticipant, InstructionPathContext,
    InstructionRootKind, InstructionSampleUsage, InstructionSnapshot, InstructionSnapshotSource,
    InstructionTicketAssociation, MAX_INSTRUCTION_PARTICIPANTS, MAX_INSTRUCTION_PATH_BYTES,
    NativeInstructionPath,
};
use dudes_proto::instruction_state::InstructionSnapshotEvidence;
use dudes_proto::repository::{HireRepositories, RepositoryTurnGrant};
use dudes_proto::repository_instruction::{
    INSTRUCTION_BATCH_VERSION, RepositoryInstructionBinding, RepositoryInstructionSnapshot,
    RepositoryInstructionStart,
};
use dudes_supervisor::input::InstructionRequest;
use dudes_supervisor::{AdoptionKey, Effect, Input, InstructionSource};
use dudes_types::{AgentId, Mode, ModelSelection, Timestamp, WorktreeId};

use dudes_supervisor::host::{MAX_OWNED_TARGET_QUERY_ROWS, OwnedTargetState};

use super::instruction_plan::{
    InstructionEvidenceDigests, InstructionPlanFacts, ProjectInstructionContext,
    instruction_participant,
};
use super::native_boundaries::same_key;
use crate::instruction_sample::{self, SAMPLE_DEADLINE, digest};
use crate::ports::Handle;

#[cfg(all(test, unix))]
#[path = "instruction_sampling/repository_tests.rs"]
mod repository_tests;

pub(super) struct LegacySampleJob {
    pub(super) request: InstructionRequest,
    pub(super) agent: AgentId,
    candidate_cwd: Option<PathBuf>,
    pub(super) sources: Vec<InstructionSource>,
    pub(super) membership_unknown: bool,
    candidate_context_unknown: bool,
    pub(super) uncertain_folders: Vec<PathBuf>,
    pub(super) selection: ModelSelection,
    pub(super) baseline: Option<Box<InstructionSnapshot>>,
    pub(super) ticket: Option<InstructionTicketAssociation>,
    pub(super) boot: Option<InstructionBootIdentity>,
    pub(super) pid_namespace: Option<String>,
    pub(super) queued: Instant,
}

const MAX_CURRENT_CENSUS_ENTRIES: usize = 512;

struct CurrentMember {
    agent: AgentId,
    cwd: PathBuf,
    mode: Mode,
    lease: Option<WorktreeId>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct NativeSourceIdentity {
    agent: AgentId,
    pid: u32,
    start_time: u64,
    boot: Option<String>,
    cwd: PathBuf,
    attached: bool,
}

impl NativeSourceIdentity {
    fn new(agent: AgentId, key: &AdoptionKey, cwd: &Path, mode: Mode) -> Self {
        Self {
            agent,
            pid: key.pid,
            start_time: key.start_time,
            boot: key.boot.clone(),
            cwd: cwd.to_path_buf(),
            attached: mode == Mode::Attached,
        }
    }
}

#[derive(Default)]
struct CurrentCensus {
    examined: usize,
    members: Vec<CurrentMember>,
    seen: BTreeSet<(AgentId, PathBuf, bool, Option<WorktreeId>)>,
    membership_unknown: bool,
    candidate_context_unknown: bool,
    uncertain_folders: BTreeSet<PathBuf>,
    captured_native: BTreeSet<NativeSourceIdentity>,
}

impl CurrentCensus {
    fn recorded(
        &mut self,
        world: &dudes_supervisor::World,
        agent: AgentId,
        mode: Mode,
        uncertain: bool,
    ) {
        if let Some(row) = world.agents.get(&agent)
            && let HireRepositories::Scoped(hire) = &row.repositories
        {
            for member in hire.repositories.entries() {
                if self.room().is_err() {
                    break;
                }
                self.member(
                    agent,
                    Some(&member.value.recorded.execution.directory),
                    mode,
                    Some(member.value.recorded.worktree),
                    uncertain,
                );
            }
        } else {
            let (path, lease) = recorded_path(world, agent)
                .map_or((None, None), |(path, lease)| (Some(path), lease));
            self.member(agent, path, mode, lease, uncertain);
        }
    }

    fn evidence(
        &mut self,
        owner: InstructionObservationOwner,
        evidence: Option<&InstructionSnapshotEvidence>,
        uncertain: bool,
    ) {
        let Some(evidence) = evidence else {
            self.membership_unknown = true;
            return;
        };
        for (_, sample) in evidence.samples() {
            if self.room().is_err() {
                break;
            }
            let _ = self.snapshot(owner, Some(sample), uncertain);
        }
    }

    fn native_evidence(
        &mut self,
        owner: InstructionObservationOwner,
        evidence: Option<&InstructionSnapshotEvidence>,
        key: &AdoptionKey,
        uncertain: bool,
    ) {
        let Some(evidence) = evidence else {
            self.membership_unknown = true;
            return;
        };
        for (_, sample) in evidence.samples() {
            if self.room().is_err() {
                break;
            }
            self.native_snapshot(owner, Some(sample), key, uncertain);
        }
    }

    fn room(&mut self) -> Result<(), ()> {
        if self.examined == MAX_CURRENT_CENSUS_ENTRIES {
            self.membership_unknown = true;
            return Err(());
        }
        self.examined += 1;
        Ok(())
    }

    fn member(
        &mut self,
        agent: AgentId,
        cwd: Option<&Path>,
        mode: Mode,
        lease: Option<WorktreeId>,
        uncertain: bool,
    ) {
        let Some(cwd) = cwd
            .filter(|path| path.as_os_str().as_encoded_bytes().len() <= MAX_INSTRUCTION_PATH_BYTES)
        else {
            self.membership_unknown = true;
            return;
        };
        let ambiguous = self.members.iter().any(|member| {
            member.agent == agent
                && member.cwd == cwd
                && (member.mode != mode || member.lease != lease)
        });
        if uncertain || ambiguous {
            self.uncertain_folders.insert(cwd.to_path_buf());
        }
        if self
            .seen
            .insert((agent, cwd.to_path_buf(), mode == Mode::Attached, lease))
        {
            self.members.push(CurrentMember {
                agent,
                cwd: cwd.to_path_buf(),
                mode,
                lease,
            });
        }
    }

    fn snapshot(
        &mut self,
        owner: InstructionObservationOwner,
        snapshot: Option<&InstructionSnapshot>,
        uncertain: bool,
    ) -> Option<PathBuf> {
        let Some(snapshot) = snapshot
            .filter(|snapshot| snapshot.source != InstructionSnapshotSource::MissingBaseline)
        else {
            self.membership_unknown = true;
            return None;
        };
        let agent = owner_agent(owner);
        let path = snapshot
            .execution_folder
            .as_ref()
            .and_then(|path| path.to_path().ok());
        let lease = path.as_ref().and_then(|path| {
            snapshot
                .participants
                .iter()
                .find(|participant| participant.agent == agent)
                .and_then(|participant| {
                    participant.discovery.roots.iter().find(|root| {
                        root.kind == InstructionRootKind::Execution
                            && root.path.to_path().ok().as_ref() == Some(path)
                    })
                })
                .and_then(|root| match root.context {
                    InstructionPathContext::OwnedWorktree { lease } => Some(lease),
                    _ => None,
                })
        });
        self.member(agent, path.as_deref(), owner_mode(owner), lease, uncertain);
        path
    }

    fn native_snapshot(
        &mut self,
        owner: InstructionObservationOwner,
        snapshot: Option<&InstructionSnapshot>,
        key: &AdoptionKey,
        uncertain: bool,
    ) {
        if let Some(path) = self.snapshot(owner, snapshot, uncertain) {
            self.captured_native.insert(NativeSourceIdentity::new(
                owner_agent(owner),
                key,
                &path,
                owner_mode(owner),
            ));
        }
    }

    fn has_native_key(&self, agent: AgentId, key: &AdoptionKey, mode: Mode) -> bool {
        self.captured_native.iter().any(|recorded| {
            recorded.agent == agent
                && recorded.pid == key.pid
                && recorded.start_time == key.start_time
                && recorded.boot == key.boot
                && recorded.attached == (mode == Mode::Attached)
        })
    }

    fn physical_member(
        &mut self,
        world: &dudes_supervisor::World,
        agent: AgentId,
        receipt: Option<(&AdoptionKey, &Path)>,
        mode: Mode,
        uncertain: bool,
    ) {
        if !receipt.is_some_and(|(key, path)| {
            self.captured_native
                .contains(&NativeSourceIdentity::new(agent, key, path, mode))
        }) {
            self.membership_unknown = true;
        }
        let cwd = receipt.map(|(_, path)| path);
        let lease = cwd.and_then(|cwd| recorded_lease(world, agent, cwd));
        self.member(agent, cwd, mode, lease, uncertain);
    }
}

fn current_recorded_census(
    world: &dudes_supervisor::World,
    owner: InstructionObservationOwner,
    baseline: Option<&InstructionSnapshot>,
    actual_cwd: Option<&Path>,
    absent: &[AdoptionKey],
) -> CurrentCensus {
    let mut census = CurrentCensus::default();
    let candidate = owner_agent(owner);
    let _ = census.room();
    if let Some(actual) = actual_cwd {
        let recorded = recorded_path(world, candidate);
        let lease = recorded.and_then(|(path, lease)| (path == actual).then_some(lease).flatten());
        census.candidate_context_unknown = recorded.is_none_or(|(path, _)| path != actual);
        census.member(
            candidate,
            Some(actual),
            owner_mode(owner),
            lease,
            census.candidate_context_unknown,
        );
        if baseline.is_some() && census.room().is_ok() {
            let _ = census.snapshot(owner, baseline, false);
        }
    } else if baseline.is_some() {
        let _ = census.snapshot(owner, baseline, false);
    } else {
        census.recorded(world, candidate, owner_mode(owner), false);
        if matches!(owner, InstructionObservationOwner::AttachedTurn { .. }) {
            census.membership_unknown = true;
        }
    }
    for agent in world.active_folder_user_ids() {
        if census.room().is_err() {
            break;
        }
        let mode = if world.agents.get(agent).is_some_and(|row| row.attached) {
            Mode::Attached
        } else {
            Mode::Managed
        };
        census.recorded(world, *agent, mode, false);
    }
    for agent in world.instructions.unresolved_owners() {
        if census.room().is_err() {
            break;
        }
        for (_, attempt) in world.instructions.unresolved_launch_attempts(agent) {
            if census.room().is_err() {
                break;
            }
            census.evidence(attempt.owner, attempt.snapshot.as_ref(), true);
        }
    }
    add_native_receipts(&mut census, world, absent);
    add_open_turn_owners(&mut census, world);
    for (_, attempt) in world.instructions.unfinished() {
        if census.room().is_err() {
            break;
        }
        census.evidence(attempt.owner, attempt.snapshot.as_ref(), false);
    }
    for (agent, key) in world.attached_children() {
        if census.room().is_err() {
            break;
        }
        if !census.has_native_key(agent, key, Mode::Attached) {
            census.membership_unknown = true;
            census.recorded(world, agent, Mode::Attached, true);
        }
    }
    census
}

fn add_native_receipts(
    census: &mut CurrentCensus,
    world: &dudes_supervisor::World,
    absent: &[AdoptionKey],
) {
    for (started, receipt) in world.managed_native_spawn_receipts() {
        if census.room().is_err() {
            break;
        }
        let Some(attempt) = world.instructions.attempt(*started).filter(|attempt| {
            attempt.owner == receipt.owner && attempt.spawn_lsn == Some(receipt.spawn_lsn)
        }) else {
            census.membership_unknown = true;
            continue;
        };
        // A child the host already proves gone is not a lingering process even when its
        // settlement record has not been appended yet. Without this the census depends on
        // whether the settlement ran before the sample.
        let gone = absent.iter().any(|key| same_key(key, &receipt.key));
        let retained = !gone
            && (attempt.finished.is_some()
                || world
                    .agents
                    .get(&owner_agent(receipt.owner))
                    .is_none_or(|row| row.retired)
                || match receipt.owner {
                    InstructionObservationOwner::Managed { turn, .. } => {
                        world.turns.get(&turn).is_none_or(|row| {
                            !row.is_open_child() || row.spawn_lsn != Some(receipt.spawn_lsn)
                        })
                    }
                    _ => true,
                });
        census.native_evidence(
            receipt.owner,
            attempt.snapshot.as_ref(),
            &receipt.key,
            retained,
        );
    }
    for (started, receipt) in world.attached_native_spawn_receipts() {
        if census.room().is_err() {
            break;
        }
        let Some(attempt) = world.instructions.attempt(*started).filter(|attempt| {
            attempt.owner == receipt.owner && attempt.spawn_lsn == Some(receipt.spawn_lsn)
        }) else {
            census.membership_unknown = true;
            continue;
        };
        let retained = attempt.finished.is_some()
            || world
                .agents
                .get(&owner_agent(receipt.owner))
                .is_none_or(|row| row.retired || !row.attached)
            || match receipt.owner {
                InstructionObservationOwner::AttachedProcess {
                    agent,
                    attachment_lsn,
                } => {
                    world.instructions.attachment_lsn(agent) != Some(attachment_lsn)
                        || world
                            .instructions
                            .attachment_end_lsn(attachment_lsn)
                            .is_some()
                }
                _ => true,
            };
        census.native_evidence(
            receipt.owner,
            attempt.snapshot.as_ref(),
            &receipt.key,
            retained,
        );
    }
}

fn add_open_turn_owners(census: &mut CurrentCensus, world: &dudes_supervisor::World) {
    for agent in world.open_turn_owner_ids() {
        if census.room().is_err() {
            break;
        }
        let attempt = world
            .agents
            .get(agent)
            .and_then(|row| row.open_turn)
            .and_then(|turn| world.instructions.current_for_turn(turn));
        if let Some((_, attempt)) = attempt {
            census.evidence(attempt.owner, attempt.snapshot.as_ref(), false);
        } else {
            if world
                .agents
                .get(agent)
                .and_then(|row| row.open_turn)
                .and_then(|turn| world.turns.get(&turn))
                .and_then(dudes_supervisor::world::TurnRow::adoption_key)
                .is_some_and(|key| census.has_native_key(*agent, &key, Mode::Managed))
            {
                continue;
            }
            census.membership_unknown = true;
            census.recorded(world, *agent, Mode::Managed, true);
        }
    }
}

fn bind_source(
    census: &mut CurrentCensus,
    member: CurrentMember,
    source: &mut InstructionSource,
    candidate: AgentId,
    candidate_cwd: Option<&Path>,
    baseline: Option<&InstructionSnapshot>,
) {
    if let Some(before) = baseline {
        let changed = source.cwd != member.cwd
            || source.mode != member.mode
            || source.lease != member.lease
            || before
                .participants
                .iter()
                .find(|old| old.agent == member.agent)
                .is_some_and(|old| old.provider != source.provider);
        if changed {
            census.uncertain_folders.insert(member.cwd.clone());
            census.candidate_context_unknown |=
                member.agent == candidate && candidate_cwd == Some(member.cwd.as_path());
        }
    }
    source.cwd = member.cwd;
    source.mode = member.mode;
    source.lease = member.lease;
}

fn owner_agent(owner: InstructionObservationOwner) -> AgentId {
    match owner {
        InstructionObservationOwner::Managed { agent, .. }
        | InstructionObservationOwner::AttachedProcess { agent, .. }
        | InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
    }
}

fn owner_mode(owner: InstructionObservationOwner) -> Mode {
    match owner {
        InstructionObservationOwner::Managed { .. } => Mode::Managed,
        InstructionObservationOwner::AttachedProcess { .. }
        | InstructionObservationOwner::AttachedTurn { .. } => Mode::Attached,
    }
}

fn recorded_path(
    world: &dudes_supervisor::World,
    agent: AgentId,
) -> Option<(&Path, Option<WorktreeId>)> {
    let row = world.agents.get(&agent)?;
    let repository = row.repositories.singleton()?;
    Some(match repository.worktree() {
        Some(lease) => (world.worktrees.get(&lease)?.path.as_path(), Some(lease)),
        None => (repository.cwd(), None),
    })
}

fn recorded_lease(
    world: &dudes_supervisor::World,
    agent: AgentId,
    cwd: &Path,
) -> Option<WorktreeId> {
    let row = world.agents.get(&agent)?;
    match &row.repositories {
        HireRepositories::Legacy(_) => recorded_path(world, agent)
            .and_then(|(recorded, lease)| (recorded == cwd).then_some(lease).flatten()),
        HireRepositories::Scoped(hire) => hire
            .repositories
            .entries()
            .iter()
            .find(|member| member.value.recorded.execution.directory == cwd)
            .map(|member| member.value.recorded.worktree),
    }
}

fn recorded_repository_hire<'a>(
    world: &'a dudes_supervisor::World,
    owner: InstructionObservationOwner,
    grant: &RepositoryTurnGrant,
) -> Option<(dudes_types::Lsn, &'a HireRepositories)> {
    let issued = match owner {
        InstructionObservationOwner::Managed { turn, .. } => world.turns.get(&turn),
        _ => None,
    };
    if let Some(issued) = issued {
        issued
            .repositories
            .as_ref()
            .filter(|scope| scope.grant() == Some(grant))
            .map(|scope| (scope.hired_lsn(), scope.hire()))
    } else {
        world
            .agents
            .get(&owner_agent(owner))
            .map(|row| (row.mcp.hire_lsn, &row.repositories))
    }
}

fn scoped_membership_unknown(
    unknown: bool,
    folders: &[PathBuf],
    candidate: &Path,
    deadline: Instant,
) -> bool {
    unknown
        || folders.iter().any(|path| {
            Instant::now() >= deadline || canonical(path).is_none_or(|folder| folder == candidate)
        })
}

impl LegacySampleJob {
    pub(super) fn queued(mut self) -> Self {
        self.queued = Instant::now();
        self
    }
}

type BoundarySample = (InstructionSnapshot, Option<InstructionTicketAssociation>);

pub(crate) struct InstructionSampler {
    requests: mpsc::Sender<QueuedSample>,
    now: Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

enum SampleJob {
    Legacy(LegacySampleJob),
    Repositories(RepositorySampleJob),
}

struct RepositoryMemberJob {
    repo_id: dudes_proto::repository::RepoId,
    root: dudes_proto::record::FileIdentity,
    job: LegacySampleJob,
}

struct RepositorySampleJob {
    request: InstructionRequest,
    owner: InstructionObservationOwner,
    binding: RepositoryInstructionBinding,
    ticket: Option<InstructionTicketAssociation>,
    members: Vec<RepositoryMemberJob>,
    baseline: Option<Box<RepositoryInstructionStart>>,
    queued: Instant,
}

#[derive(Clone)]
enum SampleProgress {
    Legacy(Arc<Mutex<InstructionSnapshot>>),
    Repositories(Arc<Mutex<instruction_sample::batch::Progress>>),
}

enum SampleEndpoint {
    Legacy(InstructionSnapshot),
    Repositories(RepositoryInstructionStart),
}

enum SampleCompletion {
    Legacy {
        request: InstructionRequest,
        ticket: Option<InstructionTicketAssociation>,
        baseline: Option<Box<InstructionSnapshot>>,
    },
    Repositories {
        request: InstructionRequest,
        baseline: Option<Box<RepositoryInstructionStart>>,
    },
}

impl SampleCompletion {
    fn input(self, endpoint: SampleEndpoint) -> Input {
        match (self, endpoint) {
            (
                Self::Legacy {
                    request,
                    ticket,
                    baseline,
                },
                SampleEndpoint::Legacy(snapshot),
            ) => {
                let comparison = baseline
                    .as_ref()
                    .map(|before| Box::new(instruction_sample::compare(before, &snapshot)));
                Input::InstructionSampled {
                    request,
                    snapshot: Box::new(snapshot),
                    ticket,
                    comparison,
                }
            }
            (Self::Repositories { request, baseline }, SampleEndpoint::Repositories(sample)) => {
                let comparisons = baseline
                    .as_ref()
                    .map(|before| instruction_sample::batch::compare(before, &sample));
                Input::RepositoryInstructionsSampled {
                    request,
                    sample: Box::new(sample),
                    comparisons,
                }
            }
            _ => unreachable!("sample correlation retains its scope"),
        }
    }
}

impl SampleProgress {
    fn deadline(&self, sampled_at: Timestamp) -> SampleEndpoint {
        match self {
            Self::Legacy(progress) => {
                SampleEndpoint::Legacy(deadline_snapshot(progress, sampled_at))
            }
            Self::Repositories(progress) => SampleEndpoint::Repositories(
                instruction_sample::batch::deadline(progress, sampled_at),
            ),
        }
    }
}

impl SampleJob {
    fn deadline(&self) -> Instant {
        (match self {
            Self::Legacy(job) => job.queued,
            Self::Repositories(job) => job.queued,
        }) + SAMPLE_DEADLINE
    }

    fn completion(&self) -> SampleCompletion {
        match self {
            Self::Legacy(job) => SampleCompletion::Legacy {
                request: job.request,
                ticket: job.ticket,
                baseline: job.baseline.clone(),
            },
            Self::Repositories(job) => SampleCompletion::Repositories {
                request: job.request,
                baseline: job.baseline.clone(),
            },
        }
    }

    fn observe(self, progress: SampleProgress, sampled_at: Timestamp) -> SampleEndpoint {
        let deadline = self.deadline();
        match (self, progress) {
            (Self::Legacy(job), SampleProgress::Legacy(progress)) => {
                let seed = instruction_sample::bounded_seed(assemble(&job, sampled_at));
                *progress.lock().unwrap_or_else(PoisonError::into_inner) = seed.clone();
                SampleEndpoint::Legacy(instruction_sample::observe_reporting(
                    seed,
                    Some(progress),
                    deadline,
                ))
            }
            (Self::Repositories(job), SampleProgress::Repositories(progress)) => {
                let seed = assemble_repositories(&job, sampled_at);
                SampleEndpoint::Repositories(instruction_sample::batch::observe_reporting(
                    seed,
                    &progress,
                    deadline,
                    job.baseline.is_none(),
                ))
            }
            _ => unreachable!("sample progress retains its scope"),
        }
    }
}

struct QueuedSample {
    job: SampleJob,
    correlation: Arc<()>,
    progress: SampleProgress,
    reply: Option<mpsc::SyncSender<BoundarySample>>,
}

struct Work {
    job: SampleJob,
    correlation: Arc<()>,
    progress: SampleProgress,
}

impl InstructionSampler {
    pub(crate) fn new(
        handle: Handle,
        now: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> std::io::Result<Self> {
        // Unbounded on purpose: a full queue is not a deadline. Each start is bounded by its ticket,
        // and a sample that waits too long still ends at its own deadline.
        let (requests, manager) = mpsc::channel::<QueuedSample>();
        let (work, worker) = mpsc::sync_channel::<Work>(1);
        let (results, completed) = mpsc::channel();
        let worker_now = Arc::clone(&now);
        std::thread::Builder::new()
            .name("dudes-instructions-io".into())
            .spawn(move || {
                while let Ok(Work {
                    job,
                    correlation,
                    progress,
                }) = worker.recv()
                {
                    let endpoint = job.observe(progress, worker_now());
                    let _ = results.send((correlation, endpoint));
                }
            })?;
        let manager_now = Arc::clone(&now);
        std::thread::Builder::new()
            .name("dudes-instructions-deadline".into())
            .spawn(move || {
                while let Ok(QueuedSample {
                    job,
                    correlation,
                    progress,
                    reply,
                }) = manager.recv()
                {
                    let completion = job.completion();
                    let deadline = job.deadline();
                    let sent = work
                        .try_send(Work {
                            job,
                            correlation: Arc::clone(&correlation),
                            progress: progress.clone(),
                        })
                        .is_ok();
                    let endpoint = await_endpoint(&completed, &correlation, deadline, sent)
                        .unwrap_or_else(|| progress.deadline(manager_now()));
                    if let Some(reply) = reply {
                        if let (
                            SampleCompletion::Legacy { ticket, .. },
                            SampleEndpoint::Legacy(snapshot),
                        ) = (completion, endpoint)
                        {
                            let _ = reply.try_send((snapshot, ticket));
                        }
                    } else {
                        handle.input(completion.input(endpoint));
                    }
                }
            })?;
        Ok(Self { requests, now })
    }

    fn queued(&self, job: SampleJob) -> QueuedSample {
        let (job, progress) = match job {
            SampleJob::Legacy(job) => {
                let job = job.queued();
                let progress =
                    SampleProgress::Legacy(Arc::new(Mutex::new(unknown_seed(&job, (self.now)()))));
                (SampleJob::Legacy(job), progress)
            }
            SampleJob::Repositories(mut job) => {
                job.queued = Instant::now();
                for member in &mut job.members {
                    member.job.queued = job.queued;
                }
                let sample = repository_unknown_seed(&job, (self.now)());
                let progress = SampleProgress::Repositories(Arc::new(Mutex::new(
                    instruction_sample::batch::Progress {
                        sample,
                        completed: 0,
                    },
                )));
                (SampleJob::Repositories(job), progress)
            }
        };
        QueuedSample {
            job,
            correlation: Arc::new(()),
            progress,
            reply: None,
        }
    }

    fn submit_job(&self, job: SampleJob) -> Result<(), Box<SampleJob>> {
        self.requests
            .send(self.queued(job))
            .map_err(|error| Box::new(error.0.job))
    }

    pub(super) fn submit(&self, job: LegacySampleJob) -> Result<(), Box<LegacySampleJob>> {
        self.submit_job(SampleJob::Legacy(job))
            .map_err(|job| match *job {
                SampleJob::Legacy(job) => Box::new(job),
                SampleJob::Repositories(_) => unreachable!("legacy submission"),
            })
    }

    fn boundary(&self, job: LegacySampleJob) -> BoundarySample {
        let (reply, endpoint) = mpsc::sync_channel(1);
        let mut queued = self.queued(SampleJob::Legacy(job));
        queued.reply = Some(reply);
        let deadline = queued.job.deadline();
        let SampleCompletion::Legacy { ticket, .. } = queued.job.completion() else {
            unreachable!("legacy boundary")
        };
        let progress = queued.progress.clone();
        if self.requests.send(queued).is_ok()
            && let Ok(result) =
                endpoint.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            return result;
        }
        let SampleEndpoint::Legacy(snapshot) = progress.deadline((self.now)()) else {
            unreachable!("legacy boundary")
        };
        (snapshot, ticket)
    }
}

fn await_endpoint<T>(
    completed: &mpsc::Receiver<(Arc<()>, T)>,
    correlation: &Arc<()>,
    deadline: Instant,
    sent: bool,
) -> Option<T> {
    while sent && Instant::now() < deadline {
        match completed.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((id, snapshot)) if Arc::ptr_eq(&id, correlation) => return Some(snapshot),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

fn deadline_snapshot(
    progress: &Mutex<InstructionSnapshot>,
    sampled_at: Timestamp,
) -> InstructionSnapshot {
    let mut snapshot = progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    snapshot.sampled_at = sampled_at;
    let _ = snapshot.try_push_gap(InstructionCoverageGap {
        origin: dudes_proto::InstructionGapOrigin::Observed,
        reason: InstructionGapKind::Deadline,
        context: InstructionPathContext::Unresolved,
        agent: None,
        path: None,
    });
    snapshot
}

fn unknown_seed(job: &LegacySampleJob, sampled_at: Timestamp) -> InstructionSnapshot {
    InstructionSnapshot {
        sampled_at,
        source: InstructionSnapshotSource::Fresh,
        execution_folder: None,
        participants: Vec::new(),
        inputs: Vec::new(),
        inventoried: Vec::new(),
        gaps: vec![InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::PathUnresolved,
            context: InstructionPathContext::Unresolved,
            agent: Some(job.agent),
            path: None,
        }],
        usage: InstructionSampleUsage::default(),
        pid_namespace: job.pid_namespace.clone(),
        boot: job.boot.clone(),
    }
}

fn repository_unknown_seed(
    job: &RepositorySampleJob,
    sampled_at: Timestamp,
) -> RepositoryInstructionStart {
    RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding: job.binding,
        owner: job.owner,
        ticket: job.ticket,
        members: job
            .members
            .iter()
            .map(|member| RepositoryInstructionSnapshot {
                repo_id: member.repo_id.clone(),
                root: None,
                snapshot: unknown_seed(&member.job, sampled_at),
            })
            .collect(),
    }
}

fn assemble_repositories(
    job: &RepositorySampleJob,
    sampled_at: Timestamp,
) -> RepositoryInstructionStart {
    let mut sample = repository_unknown_seed(job, sampled_at);
    for (index, member) in job.members.iter().enumerate() {
        let before = job
            .baseline
            .as_ref()
            .and_then(|before| before.members.get(index));
        let path = member
            .job
            .candidate_cwd
            .as_deref()
            .and_then(canonical)
            .filter(|canonical| Some(canonical.as_path()) == member.job.candidate_cwd.as_deref());
        let root = path
            .as_deref()
            .and_then(|path| crate::worktree::trust::identity(path).ok());
        let starting = before.is_none();
        if starting && root != Some(member.root) {
            continue;
        }
        let mut snapshot = if before.is_some_and(|before| before.root.is_none()) {
            unknown_seed(&member.job, sampled_at)
        } else {
            assemble(&member.job, sampled_at)
        };
        if let Some(before) = before {
            let current = std::mem::take(&mut snapshot.participants);
            snapshot.participants = before
                .snapshot
                .participants
                .iter()
                .map(|frozen| {
                    let mut participant = frozen.clone();
                    if let Some(now) = current.iter().find(|now| now.agent == frozen.agent) {
                        participant.declaration_digest = now.declaration_digest;
                    }
                    participant
                })
                .collect();
        }
        snapshot.execution_folder = path
            .as_deref()
            .filter(|_| root.is_some())
            .and_then(|path| NativeInstructionPath::from_path(path).ok());
        if root != Some(member.root) {
            let _ = snapshot.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: if root.is_none() {
                    InstructionGapKind::PathUnresolved
                } else {
                    InstructionGapKind::ChangedDuringRead
                },
                context: InstructionPathContext::Unresolved,
                agent: Some(member.job.agent),
                path: None,
            });
        }
        sample.members[index].root = root;
        sample.members[index].snapshot = instruction_sample::bounded_seed(snapshot);
        while !instruction_sample::batch::seed_fits(&sample) {
            let snapshot = &mut sample.members[index].snapshot;
            if snapshot.participants.pop().is_some() {
                let _ = snapshot.try_push_gap(InstructionCoverageGap::metadata_limit());
            } else {
                *snapshot = unknown_seed(&member.job, sampled_at);
                snapshot.execution_folder = path
                    .as_deref()
                    .filter(|_| root.is_some())
                    .and_then(|path| NativeInstructionPath::from_path(path).ok());
                let _ = snapshot.try_push_gap(InstructionCoverageGap::metadata_limit());
                break;
            }
        }
    }
    assert!(
        instruction_sample::batch::seed_fits(&sample),
        "bounded all-hire discovery plan"
    );
    sample
}

fn canonical(path: &Path) -> Option<std::path::PathBuf> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    std::fs::metadata(&canonical)
        .ok()?
        .is_dir()
        .then_some(canonical)
}

fn assemble(job: &LegacySampleJob, sampled_at: Timestamp) -> InstructionSnapshot {
    let mut snapshot = unknown_seed(job, sampled_at);
    let Some(candidate) = job.candidate_cwd.as_deref().and_then(canonical) else {
        return snapshot;
    };
    let Ok(folder) = NativeInstructionPath::from_path(&candidate) else {
        return snapshot;
    };
    snapshot.execution_folder = Some(folder);
    snapshot.gaps.clear();
    let mut members: Vec<&InstructionSource> = Vec::new();
    let mut incomplete = scoped_membership_unknown(
        job.membership_unknown,
        &job.uncertain_folders,
        &candidate,
        job.queued + SAMPLE_DEADLINE,
    );
    for source in &job.sources {
        if Instant::now() >= job.queued + SAMPLE_DEADLINE {
            incomplete = true;
            break;
        }
        match canonical(&source.cwd) {
            Some(folder) if folder == candidate => {
                if let Some(existing) = members
                    .iter()
                    .find(|existing| existing.agent == source.agent)
                {
                    incomplete |= existing.mode != source.mode || existing.lease != source.lease;
                } else {
                    members.push(source);
                }
            }
            Some(_) => {}
            None => incomplete = true,
        }
    }
    if incomplete {
        let _ = snapshot.try_push_gap(InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::ParticipantCoverageUnknown,
            context: InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        });
    }
    let evidence = InstructionEvidenceDigests {
        claude_documentation: digest(b"b9e2adb783629c9030f897ba768f18af1c8b7652e37df4faf333f75c2f2aab3f:ae89fa20fc67002e612006ce80d2039e76df48d6e929b6ae148fc3cb25238005"),
        codex_source: digest(b"97427a870741b904a3f3c143f5990eaaa65b5bfa99ddac1808b2980052b5fc78"),
        pi_source: digest(b"dbe87c11983b8dac949377904f4cf0511155771498ba88d2fefb0cd0d5dd9e28"),
        daemon_composition: digest(b"dudes.native-instruction-composition.v1"),
    };
    for source in members.iter().take(MAX_INSTRUCTION_PARTICIPANTS) {
        let declaration = source.declaration(&candidate, &job.selection);
        let context = if let Some(lease) = source.lease {
            ProjectInstructionContext::OwnedWorktree(lease)
        } else if members.len() == 1 && !incomplete {
            ProjectInstructionContext::UnsharedAfterCompleteCensus
        } else {
            ProjectInstructionContext::Shared
        };
        let facts = InstructionPlanFacts {
            canonical_execution_folder: &candidate,
            project_context: context,
            evidence: &evidence,
            daemon_configuration_digest: None,
            codex_guard: None,
        };
        let mut participant =
            instruction_participant(source.agent, source.provider, &declaration, &facts);
        if job.candidate_context_unknown && source.agent == job.agent {
            unresolved_project_context(&mut participant);
        }
        snapshot.participants.push(participant);
        if source.agent != job.agent || job.candidate_context_unknown {
            let _ = snapshot.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::ConfigurationUnknown,
                context: InstructionPathContext::Unresolved,
                agent: Some(source.agent),
                path: None,
            });
        }
    }
    if members.len() > MAX_INSTRUCTION_PARTICIPANTS {
        let _ = snapshot.try_push_gap(InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::ParticipantLimit,
            context: InstructionPathContext::SharedProject,
            agent: None,
            path: None,
        });
    }
    merge_baseline(&mut snapshot, job.baseline.as_deref());
    snapshot
}

fn unresolved_project_context(participant: &mut InstructionParticipant) {
    for root in &mut participant.discovery.roots {
        if matches!(
            root.context,
            InstructionPathContext::OwnedWorktree { .. }
                | InstructionPathContext::UnsharedProject
                | InstructionPathContext::SharedProject
        ) {
            root.context = InstructionPathContext::Unresolved;
        }
    }
}

fn merge_baseline(snapshot: &mut InstructionSnapshot, baseline: Option<&InstructionSnapshot>) {
    if let Some(before) = baseline {
        let current = std::mem::take(&mut snapshot.participants);
        for old in &before.participants {
            let mut frozen = old.clone();
            match current
                .iter()
                .find(|participant| participant.agent == old.agent)
            {
                Some(now) => {
                    if old.provider != now.provider
                        || old.mode != now.mode
                        || old.declaration_digest != now.declaration_digest
                        || old.discovery.configuration_digest != now.discovery.configuration_digest
                    {
                        let _ = snapshot.try_push_gap(InstructionCoverageGap {
                            origin: dudes_proto::InstructionGapOrigin::Observed,
                            reason: InstructionGapKind::ConfigurationUnknown,
                            context: InstructionPathContext::Unresolved,
                            agent: Some(old.agent),
                            path: None,
                        });
                    }
                    frozen.declaration_digest = now.declaration_digest;
                    for (old_root, current_root) in
                        frozen.discovery.roots.iter_mut().zip(&now.discovery.roots)
                    {
                        if old_root.path == current_root.path
                            && (old_root.context == InstructionPathContext::UnsharedProject
                                || current_root.context == InstructionPathContext::Unresolved)
                        {
                            old_root.context = current_root.context;
                        }
                    }
                    frozen
                        .discovery
                        .gaps
                        .extend(now.discovery.gaps.iter().cloned());
                }
                None => {
                    let _ = snapshot.try_push_gap(InstructionCoverageGap {
                        origin: dudes_proto::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::ParticipantChanged,
                        context: InstructionPathContext::SharedProject,
                        agent: Some(old.agent),
                        path: None,
                    });
                }
            }
            snapshot.participants.push(frozen);
        }
        for participant in current {
            if !before
                .participants
                .iter()
                .any(|old| old.agent == participant.agent)
            {
                snapshot.participants.push(participant);
            }
        }
    }
}

impl<H: super::ProcessHost, P: super::PredicateRunner, C: super::Clock> super::Ctx<H, P, C> {
    fn absent_native_targets(&self, world: &dudes_supervisor::World) -> Vec<AdoptionKey> {
        if world.managed_native_spawn_receipts().next().is_none() {
            return Vec::new();
        }
        self.host
            .owned_targets(MAX_OWNED_TARGET_QUERY_ROWS)
            .targets
            .into_iter()
            .filter(|target| target.state == OwnedTargetState::NativeBoundaryAbsent)
            .filter_map(|target| target.key)
            .collect()
    }

    fn current_instruction_census(
        &self,
        world: &dudes_supervisor::World,
        owner: InstructionObservationOwner,
        baseline: Option<&InstructionSnapshot>,
        actual_cwd: Option<&Path>,
    ) -> CurrentCensus {
        let absent = self.absent_native_targets(world);
        let mut census = current_recorded_census(world, owner, baseline, actual_cwd, &absent);
        let managed_receipts: Result<(), ()> = self.children.visit_owned(|turn, key, cwd| {
            census.room()?;
            let Some(row) = world.turns.get(&turn) else {
                census.membership_unknown = true;
                return Ok(());
            };
            census.physical_member(
                world,
                row.agent,
                cwd.map(|path| (&key, path)),
                Mode::Managed,
                false,
            );
            Ok(())
        });
        census.membership_unknown |= managed_receipts.is_err();
        let uncertain: Result<bool, ()> = self.children.visit_uncertain(|turn, cwd| {
            census.room()?;
            census.membership_unknown = true;
            if let Some(row) = world.turns.get(&turn) {
                let lease = recorded_lease(world, row.agent, cwd);
                census.member(row.agent, Some(cwd), Mode::Managed, lease, true);
            }
            Ok(())
        });
        census.membership_unknown |= uncertain != Ok(false);
        for (agent, pty) in &self.live_ptys {
            if census.room().is_err() {
                break;
            }
            let receipt = self.pty_host.receipt(*pty);
            census.physical_member(world, *agent, receipt, Mode::Attached, false);
        }
        for (pty, cleanup) in &self.cleanup_ptys {
            if census.room().is_err() {
                break;
            }
            let receipt = self.pty_host.receipt(*pty);
            census.physical_member(world, cleanup.agent, receipt, Mode::Attached, true);
        }
        census
    }

    fn instruction_job<R: super::Rng>(
        &self,
        sup: &super::Supervisor<super::Shared<C>, R>,
        owner: InstructionObservationOwner,
        goal: Option<dudes_types::GoalId>,
        selection: &ModelSelection,
        baseline: Option<&InstructionSnapshot>,
        actual_cwd: Option<&Path>,
    ) -> anyhow::Result<LegacySampleJob> {
        let agent = owner_agent(owner);
        let mut census = self.current_instruction_census(sup.world(), owner, baseline, actual_cwd);
        let candidate_cwd = match (actual_cwd, baseline) {
            (Some(path), _) => Some(path.to_path_buf()),
            (None, Some(before)) => before
                .execution_folder
                .as_ref()
                .and_then(|path| path.to_path().ok()),
            (None, None) => recorded_path(sup.world(), agent).map(|(path, _)| path.to_path_buf()),
        }
        .filter(|path| path.as_os_str().as_encoded_bytes().len() <= MAX_INSTRUCTION_PATH_BYTES);
        let mut sources = Vec::new();
        for member in std::mem::take(&mut census.members) {
            match sup.instruction_source(member.agent) {
                Some(mut source) => {
                    bind_source(
                        &mut census,
                        member,
                        &mut source,
                        agent,
                        candidate_cwd.as_deref(),
                        baseline,
                    );
                    sources.push(source);
                }
                None => {
                    census.uncertain_folders.insert(member.cwd);
                }
            }
        }
        let ticket = goal
            .filter(|goal| {
                sup.world()
                    .goal_of(agent)
                    .is_some_and(|current| current.goal.id == *goal)
                    || sup
                        .world()
                        .active_repository_round(agent)
                        .ok()
                        .flatten()
                        .is_some_and(|round| round.goal == *goal)
            })
            .and_then(|goal| {
                sup.world()
                    .instructions
                    .association_for_goal(goal, dudes_types::Seq(0))
            })
            .map(|association| {
                self.projections.ticket(association.card).map(|ticket| {
                    ticket
                        .filter(|(card, ticket)| {
                            card.status != dudes_types::CardStatus::Done
                                && card.claimed_by == Some(agent)
                                && ticket.goal == Some(association.goal)
                        })
                        .map(|(card, _)| InstructionTicketAssociation {
                            seq_at_start: card.seq,
                            ..association
                        })
                })
            })
            .transpose()?
            .flatten();
        let boot = if cfg!(target_os = "linux") {
            match self.host.boot_identity() {
                dudes_types::HostBoot::Read(value) => {
                    Some(InstructionBootIdentity::LinuxKernelBootId { value })
                }
                _ => None,
            }
        } else {
            None
        };
        Ok(LegacySampleJob {
            request: InstructionRequest(0),
            agent,
            candidate_cwd,
            sources,
            membership_unknown: census.membership_unknown,
            candidate_context_unknown: census.candidate_context_unknown,
            uncertain_folders: census.uncertain_folders.into_iter().collect(),
            selection: selection.clone(),
            baseline: baseline.cloned().map(Box::new),
            ticket,
            boot,
            pid_namespace: if cfg!(target_os = "linux") {
                self.host.pid_namespace()
            } else {
                None
            },
            queued: Instant::now(),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one physical census freezes every hired member before worker dispatch"
    )]
    fn repository_instruction_job<R: super::Rng>(
        &self,
        sup: &super::Supervisor<super::Shared<C>, R>,
        request: InstructionRequest,
        owner: InstructionObservationOwner,
        grant: &RepositoryTurnGrant,
        selection: &ModelSelection,
        baseline: Option<&RepositoryInstructionStart>,
    ) -> anyhow::Result<RepositorySampleJob> {
        let agent = owner_agent(owner);
        let world = sup.world();
        let hire = recorded_repository_hire(world, owner, grant);
        let Some((hired_lsn, HireRepositories::Scoped(hire))) = hire else {
            anyhow::bail!("instruction sampling requires the exact recorded hire");
        };
        anyhow::ensure!(
            RepositoryTurnGrant::select(hired_lsn, hire, grant.ticket.as_ref()).as_ref()
                == Ok(grant),
            "instruction sampling grant changed"
        );
        let binding = RepositoryInstructionBinding {
            hired_lsn,
            ticket_start_lsn: grant.ticket.as_ref().map(|round| round.start_lsn),
        };
        let ticket = if let Some(baseline) = baseline {
            anyhow::ensure!(
                baseline.binding == binding
                    && baseline.owner == owner
                    && baseline
                        .members
                        .iter()
                        .map(|member| &member.repo_id)
                        .eq(hire
                            .repositories
                            .entries()
                            .iter()
                            .map(|member| &member.repo_id)),
                "instruction sampling baseline changed"
            );
            baseline.ticket
        } else if let Some(round) = &grant.ticket {
            let association = world
                .instructions
                .association_for_goal(round.goal, dudes_types::Seq::ZERO)
                .filter(|association| {
                    association.card == round.card
                        && world.ticket_repository_round(round.goal) == Some(round)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("frozen ticket instruction assignment unavailable")
                })?;
            let (card, _) = self
                .projections
                .ticket(round.card)?
                .filter(|(card, ticket)| {
                    card.status != dudes_types::CardStatus::Done
                        && card.claimed_by == Some(agent)
                        && ticket.goal == Some(round.goal)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("frozen ticket instruction assignment is no longer current")
                })?;
            Some(InstructionTicketAssociation {
                seq_at_start: card.seq,
                ..association
            })
        } else {
            None
        };
        let mut census = self.current_instruction_census(world, owner, None, None);
        let mut sources = Vec::new();
        for member in std::mem::take(&mut census.members) {
            if let Some(mut source) = sup.instruction_source_for_folder(member.agent, &member.cwd) {
                let before = baseline
                    .and_then(|before| {
                        before.members.iter().find(|before| {
                            before
                                .snapshot
                                .execution_folder
                                .as_ref()
                                .and_then(|path| path.to_path().ok())
                                .as_deref()
                                == Some(member.cwd.as_path())
                        })
                    })
                    .map(|before| &before.snapshot);
                bind_source(&mut census, member, &mut source, agent, None, before);
                sources.push(source);
            } else {
                census.uncertain_folders.insert(member.cwd);
            }
        }
        let boot = if cfg!(target_os = "linux") {
            match self.host.boot_identity() {
                dudes_types::HostBoot::Read(value) => {
                    Some(InstructionBootIdentity::LinuxKernelBootId { value })
                }
                _ => None,
            }
        } else {
            None
        };
        let pid_namespace = if cfg!(target_os = "linux") {
            self.host.pid_namespace()
        } else {
            None
        };
        let queued = Instant::now();
        let members = hire
            .repositories
            .entries()
            .iter()
            .map(|member| RepositoryMemberJob {
                repo_id: member.repo_id.clone(),
                root: member.value.recorded.execution.directory_identity,
                job: LegacySampleJob {
                    request,
                    agent,
                    candidate_cwd: Some(member.value.recorded.execution.directory.clone()),
                    sources: sources.clone(),
                    membership_unknown: census.membership_unknown,
                    candidate_context_unknown: false,
                    uncertain_folders: census.uncertain_folders.iter().cloned().collect(),
                    selection: selection.clone(),
                    baseline: baseline
                        .and_then(|before| {
                            before
                                .members
                                .iter()
                                .find(|before| before.repo_id == member.repo_id)
                        })
                        .map(|before| Box::new(before.snapshot.clone())),
                    ticket,
                    boot: boot.clone(),
                    pid_namespace: pid_namespace.clone(),
                    queued,
                },
            })
            .collect();
        Ok(RepositorySampleJob {
            request,
            owner,
            binding,
            ticket,
            members,
            baseline: baseline.cloned().map(Box::new),
            queued,
        })
    }

    pub(super) fn sample_instruction_boundary<R: super::Rng>(
        &self,
        sup: &super::Supervisor<super::Shared<C>, R>,
        owner: InstructionObservationOwner,
        goal: Option<dudes_types::GoalId>,
        selection: &ModelSelection,
        baseline: Option<&InstructionSnapshot>,
        actual_cwd: &Path,
    ) -> anyhow::Result<BoundarySample> {
        let job = self.instruction_job(sup, owner, goal, selection, baseline, Some(actual_cwd))?;
        Ok(self.instruction_sampler.boundary(job))
    }

    pub(super) fn sample_instructions<R: super::Rng>(
        &mut self,
        effect: &Effect,
        queue: &mut std::collections::VecDeque<Input>,
        sup: &super::Supervisor<super::Shared<C>, R>,
    ) -> anyhow::Result<()> {
        if let Effect::SampleRepositoryInstructions {
            request,
            owner,
            grant,
            selection,
            baseline,
        } = effect
        {
            let job = match self.repository_instruction_job(
                sup,
                *request,
                *owner,
                grant,
                selection,
                baseline.as_deref(),
            ) {
                Ok(job) => job,
                Err(error) => {
                    tracing::warn!(request = ?request, %error, "repository instruction request unavailable");
                    queue.push_back(Input::RepositoryInstructionsUnavailable { request: *request });
                    return Ok(());
                }
            };
            if let Err(job) = self
                .instruction_sampler
                .submit_job(SampleJob::Repositories(job))
            {
                let completion = job.completion();
                let SampleJob::Repositories(job) = *job else {
                    unreachable!("repository submission")
                };
                let sample = repository_unknown_seed(&job, self.clock.now());
                queue.push_back(completion.input(SampleEndpoint::Repositories(sample)));
            }
            return Ok(());
        }
        let Effect::SampleInstructions {
            request,
            owner,
            goal,
            selection,
            baseline,
        } = effect
        else {
            anyhow::bail!("instruction sampling requires its sample effect");
        };
        let request = *request;
        let mut job =
            self.instruction_job(sup, *owner, *goal, selection, baseline.as_deref(), None)?;
        job.request = request;
        let agent = job.agent;
        let ticket = job.ticket;
        if let Err(job) = self.instruction_sampler.submit(job) {
            let mut snapshot = unknown_seed(&job, self.clock.now());
            let _ = snapshot.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::Deadline,
                context: InstructionPathContext::Unresolved,
                agent: Some(agent),
                path: None,
            });
            let comparison = job
                .baseline
                .as_ref()
                .map(|before| Box::new(instruction_sample::compare(before, &snapshot)));
            queue.push_back(Input::InstructionSampled {
                request,
                snapshot: Box::new(snapshot),
                ticket,
                comparison,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::instruction::{
        InstructionBaselineRef, InstructionComparison, InstructionDeclarationMode,
        InstructionDiscoveryPlan, InstructionIssueAssociation, InstructionRoot,
        InstructionSettlement, InstructionUnknownReason,
    };
    use dudes_proto::{AgentBudget, Entry, Record};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use dudes_providers::adapter::{
        Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
    };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use dudes_providers::claude::{ClaudeAdapter, config::ClaudeConfig};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use dudes_providers::launch::{LaunchError, LaunchSpec};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use dudes_providers::stream::TurnReader;
    use dudes_types::{Lsn, Provider, TurnId};

    use super::*;

    fn hired(agent: AgentId, cwd: &Path) -> Record {
        Record::AgentHired {
            id: agent,
            name: "worker".into(),
            character: None,
            accent: None,
            role: "builder".into(),
            department: "engineering".into(),
            provider: Provider::Claude,
            cwd: cwd.to_path_buf(),
            workspace: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: None,
            trusted: true,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }
    }

    fn fold(world: &mut dudes_supervisor::World, lsn: u64, record: Record) {
        world.apply(
            Lsn(lsn),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record,
            },
        );
    }

    fn managed(agent: AgentId) -> InstructionObservationOwner {
        InstructionObservationOwner::Managed {
            agent,
            turn: TurnId::from_parts(1, 1),
            issue: InstructionIssueAssociation::Prospective,
        }
    }

    fn snapshot(path: &Path) -> InstructionSnapshot {
        InstructionSnapshot {
            sampled_at: Timestamp::UNIX_EPOCH,
            source: InstructionSnapshotSource::Fresh,
            pid_namespace: None,
            boot: None,
            execution_folder: Some(NativeInstructionPath::from_path(path).unwrap()),
            participants: Vec::new(),
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: vec![InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::DeclarationMissing,
                context: InstructionPathContext::Unresolved,
                agent: None,
                path: None,
            }],
            usage: InstructionSampleUsage::default(),
        }
    }

    fn job() -> LegacySampleJob {
        LegacySampleJob {
            request: InstructionRequest(0),
            agent: AgentId::from_parts(1, 1),
            candidate_cwd: Some(PathBuf::from("/current-folder")),
            sources: Vec::new(),
            membership_unknown: false,
            candidate_context_unknown: false,
            uncertain_folders: Vec::new(),
            selection: ModelSelection::provider_default(),
            baseline: None,
            ticket: None,
            boot: None,
            pid_namespace: None,
            queued: Instant::now(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct MinimalDeclarationAdapter(ClaudeAdapter);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Adapter for MinimalDeclarationAdapter {
        fn provider(&self) -> Provider {
            self.0.provider()
        }
        fn program(&self) -> &std::ffi::OsStr {
            self.0.program()
        }
        fn tier(&self) -> dudes_types::Tier {
            self.0.tier()
        }
        fn isolation(&self) -> Isolation {
            self.0.isolation()
        }
        fn session_acquisition(&self) -> SessionAcquisition {
            self.0.session_acquisition()
        }
        fn model_selection(&self) -> ModelSelection {
            self.0.model_selection()
        }
        fn managed(
            &self,
            ctx: &AgentCtx<'_>,
            task: &str,
            session: &TurnSession,
            selection: &ModelSelection,
        ) -> Result<LaunchSpec, LaunchError> {
            self.0.managed(ctx, task, session, selection)
        }
        fn turn_reader(&self) -> Box<dyn TurnReader> {
            self.0.turn_reader()
        }
        fn managed_install(
            &self,
            ctx: &AgentCtx<'_>,
            selection: &ModelSelection,
        ) -> ManagedInstall {
            self.0.managed_install(ctx, selection)
        }
        fn usage_source(&self) -> UsageSource {
            self.0.usage_source()
        }
        fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
            self.0.sandbox_flag()
        }
        fn billing_env_vars(&self) -> &'static [&'static str] {
            self.0.billing_env_vars()
        }
        fn instruction_manifest(
            &self,
            _ctx: &AgentCtx<'_>,
            mode: Mode,
            _selection: &ModelSelection,
        ) -> dudes_providers::instructions::InstructionDeclaration {
            dudes_providers::instructions::InstructionDeclaration::missing(self.provider(), mode)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assembled_participants(
        path: &Path,
        participants: usize,
        incomplete: bool,
        mut harness: dudes_supervisor::testing::Harness,
    ) -> InstructionSnapshot {
        let mut sample = job();
        sample.candidate_cwd = Some(path.to_path_buf());
        sample.membership_unknown = incomplete;
        for ordinal in 1..=participants {
            let agent = AgentId::from_parts(1, ordinal as u128);
            harness
                .supervisor
                .handle(
                    Input::Replayed {
                        lsn: Lsn(u64::try_from(ordinal).unwrap()),
                        entry: Entry {
                            at: Timestamp::UNIX_EPOCH,
                            record: hired(agent, path),
                        },
                    },
                    &dudes_supervisor::RecoveryOperation::empty(),
                )
                .unwrap();
            sample
                .sources
                .push(harness.supervisor.instruction_source(agent).unwrap());
        }
        sample.queued = Instant::now();
        assemble(&sample, Timestamp::UNIX_EPOCH)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn incomplete_two_participant_coverage_does_not_claim_a_participant_limit() {
        let folder = tempfile::tempdir().unwrap();
        let sampled = assembled_participants(
            folder.path(),
            2,
            true,
            dudes_supervisor::testing::Harness::new(),
        );
        assert_eq!(sampled.participants.len(), 2);
        assert!(sampled.gaps.iter().any(|gap| gap.reason
            == InstructionGapKind::ParticipantCoverageUnknown
            && gap.context == InstructionPathContext::Unresolved
            && gap.agent.is_none()
            && gap.path.is_none()));
        assert!(
            !sampled
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ParticipantLimit),
            "unknown participation with two recorded participants is not a participant-count overflow"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn actual_participant_overflow_keeps_the_participant_limit_gap() {
        let folder = tempfile::tempdir().unwrap();
        let harness = dudes_supervisor::testing::Harness::with_adapter(
            dudes_supervisor::testing::FakeClock::frozen(0),
            dudes_supervisor::testing::FakeHost::wall_clock_anchored(),
            || {
                Box::new(MinimalDeclarationAdapter(ClaudeAdapter::new(
                    ClaudeConfig::default(),
                )))
            },
        );
        let sampled = assembled_participants(
            folder.path(),
            MAX_INSTRUCTION_PARTICIPANTS + 1,
            false,
            harness,
        );
        assert_eq!(sampled.participants.len(), MAX_INSTRUCTION_PARTICIPANTS);
        assert!(sampled.participants.iter().all(|participant| {
            participant.mode == InstructionDeclarationMode::Unknown
                && participant
                    .discovery
                    .gaps
                    .iter()
                    .any(|gap| gap.reason == InstructionGapKind::DeclarationMissing)
        }));
        assert!(
            !sampled
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MetadataLimit),
            "the minimal unsupported declaration must isolate count overflow: {:?}",
            sampled.gaps
        );
        assert!(
            sampled
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ParticipantLimit
                    && gap.context == InstructionPathContext::SharedProject)
        );
    }

    #[test]
    fn duplicate_request_numbers_cannot_take_a_previous_jobs_endpoint() {
        let (requests, _manager) = mpsc::channel();
        let sampler = InstructionSampler {
            requests,
            now: Arc::new(|| Timestamp::UNIX_EPOCH),
        };
        let first = sampler.queued(SampleJob::Legacy(job()));
        let second = sampler.queued(SampleJob::Legacy(job()));
        assert!(matches!(
            first.job.completion(),
            SampleCompletion::Legacy {
                request: InstructionRequest(0),
                ..
            }
        ));
        assert!(matches!(
            second.job.completion(),
            SampleCompletion::Legacy {
                request: InstructionRequest(0),
                ..
            }
        ));
        assert!(!Arc::ptr_eq(&first.correlation, &second.correlation));
        let (results, completed) = mpsc::channel();
        results
            .send((first.correlation, snapshot(Path::new("/old-folder"))))
            .unwrap();
        results
            .send((
                Arc::clone(&second.correlation),
                snapshot(Path::new("/fresh-folder")),
            ))
            .unwrap();
        let received = await_endpoint(
            &completed,
            &second.correlation,
            Instant::now() + SAMPLE_DEADLINE,
            true,
        )
        .unwrap();
        assert_eq!(
            received.execution_folder.unwrap().to_path().unwrap(),
            Path::new("/fresh-folder")
        );
    }

    #[test]
    fn timeout_preserves_available_progress_and_reports_its_gap() {
        let (results, completed) = mpsc::channel();
        let correlation = Arc::new(());
        results
            .send((Arc::clone(&correlation), snapshot(Path::new("/too-late"))))
            .unwrap();
        assert!(await_endpoint(&completed, &correlation, Instant::now(), true).is_none());
        let mut observed = snapshot(Path::new("/observed-folder"));
        observed.usage.source_bytes = 76;
        let progress = Mutex::new(observed.clone());
        let timed_out = deadline_snapshot(&progress, Timestamp::UNIX_EPOCH);
        assert_eq!(timed_out.execution_folder, observed.execution_folder);
        assert_eq!(timed_out.usage.source_bytes, 76);
        assert!(timed_out.gaps.contains(&observed.gaps[0]));
        assert!(
            timed_out
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::Deadline)
        );
        assert_eq!(progress.lock().unwrap().gaps, observed.gaps);
    }

    #[test]
    fn a_burst_of_starts_is_all_sampled_in_order() {
        let (jobs, inputs) = mpsc::channel();
        let sampler =
            InstructionSampler::new(Handle::new(jobs), Arc::new(|| Timestamp::UNIX_EPOCH)).unwrap();
        for request in 0..8 {
            let mut burst = job();
            burst.request = InstructionRequest(request);
            assert!(sampler.submit(burst).is_ok(), "start {request} was refused");
        }
        for request in 0..8 {
            let crate::ports::Job::Input(input) = inputs
                .recv_timeout(std::time::Duration::from_secs(4))
                .expect("every start is sampled")
            else {
                panic!("the sampler only sends inputs");
            };
            let Input::InstructionSampled {
                request: got,
                snapshot,
                ..
            } = *input
            else {
                panic!("the sampler only reports samples");
            };
            assert_eq!(got, InstructionRequest(request));
            assert!(
                !snapshot
                    .gaps
                    .iter()
                    .any(|gap| gap.reason == InstructionGapKind::Deadline
                        && gap.context == InstructionPathContext::Unresolved),
                "start {request} was guessed: {:?}",
                snapshot.gaps
            );
        }
    }

    #[test]
    fn actual_candidate_cwd_precedes_a_different_recorded_or_frozen_folder() {
        let mut world = dudes_supervisor::World::default();
        let agent = AgentId::from_parts(1, 1);
        fold(&mut world, 1, hired(agent, Path::new("/recorded-folder")));
        let before = snapshot(Path::new("/frozen-folder"));
        let census = current_recorded_census(
            &world,
            managed(agent),
            Some(&before),
            Some(Path::new("/actual-folder")),
            &[],
        );
        assert_eq!(census.members[0].cwd, Path::new("/actual-folder"));
        assert_eq!(census.members[0].lease, None);
        assert!(census.candidate_context_unknown);
        assert!(
            census
                .uncertain_folders
                .contains(Path::new("/actual-folder"))
        );
        assert!(
            census
                .members
                .iter()
                .any(|member| member.cwd == Path::new("/frozen-folder"))
        );
        assert!(
            census
                .members
                .iter()
                .any(|member| member.cwd == Path::new("/recorded-folder"))
        );
        let matching = current_recorded_census(
            &world,
            managed(agent),
            None,
            Some(Path::new("/recorded-folder")),
            &[],
        );
        assert!(!matching.candidate_context_unknown);
    }

    fn participant(path: &Path, context: InstructionPathContext) -> InstructionParticipant {
        InstructionParticipant {
            agent: AgentId::from_parts(1, 1),
            provider: Provider::Claude,
            mode: InstructionDeclarationMode::ClaudeAttached,
            declaration_digest: digest(b"frozen declaration"),
            discovery: InstructionDiscoveryPlan {
                roots: vec![InstructionRoot {
                    path: NativeInstructionPath::from_path(path).unwrap(),
                    kind: InstructionRootKind::Execution,
                    context,
                }],
                rules: Vec::new(),
                configuration_digest: Some(digest(b"frozen configuration")),
                gaps: Vec::new(),
            },
        }
    }

    #[test]
    fn ending_snapshot_does_not_infer_a_new_lease_at_the_frozen_path() {
        let mut world = dudes_supervisor::World::default();
        let agent = AgentId::from_parts(1, 1);
        let lease = WorktreeId::from_bytes([1; 16]);
        let path = Path::new("/same-folder");
        fold(
            &mut world,
            1,
            Record::WorktreeLeased {
                worktree: lease,
                origin: "/origin".into(),
                path: path.to_path_buf(),
                branch: "dudes/test".into(),
                base: dudes_types::GitCommit::parse("0000000000000000000000000000000000000000")
                    .unwrap(),
                from: None,
            },
        );
        let mut hire = hired(agent, path);
        if let Record::AgentHired { worktree, .. } = &mut hire {
            *worktree = Some(lease);
        }
        fold(&mut world, 2, hire);
        assert_eq!(recorded_path(&world, agent).unwrap().1, Some(lease));
        let mut before = snapshot(path);
        before
            .participants
            .push(participant(path, InstructionPathContext::UnsharedProject));
        let owner = InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn: Lsn(1),
        };
        let census = current_recorded_census(&world, owner, Some(&before), None, &[]);
        assert_eq!(census.members[0].cwd, path);
        assert_eq!(census.members[0].mode, Mode::Attached);
        assert_eq!(census.members[0].lease, None);
        assert!(census.uncertain_folders.contains(path));
    }

    #[test]
    fn changed_ending_composition_stays_unknown_and_keeps_the_frozen_mode() {
        let path = Path::new("/frozen-folder");
        let mut before = snapshot(path);
        before.participants.push(participant(
            path,
            InstructionPathContext::OwnedWorktree {
                lease: WorktreeId::from_bytes([1; 16]),
            },
        ));
        let mut after = snapshot(path);
        let mut current = participant(path, InstructionPathContext::Unresolved);
        current.mode = InstructionDeclarationMode::ClaudeDefault;
        current.declaration_digest = digest(b"current declaration");
        current.discovery.configuration_digest = Some(digest(b"current configuration"));
        after.participants.push(current);
        merge_baseline(&mut after, Some(&before));
        assert_eq!(
            after.participants[0].mode,
            InstructionDeclarationMode::ClaudeAttached
        );
        assert_eq!(
            after.participants[0].discovery.roots[0].path,
            before.participants[0].discovery.roots[0].path
        );
        assert_eq!(
            after.participants[0].discovery.roots[0].context,
            InstructionPathContext::Unresolved
        );
        assert!(
            after
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown
                    && gap.agent == Some(before.participants[0].agent))
        );
    }

    #[test]
    fn unchanged_ending_declaration_does_not_invent_configuration_uncertainty() {
        let path = Path::new("/frozen-folder");
        let mut before = snapshot(path);
        before
            .participants
            .push(participant(path, InstructionPathContext::UnsharedProject));
        let mut after = before.clone();
        merge_baseline(&mut after, Some(&before));
        assert_eq!(after.participants, before.participants);
        assert!(
            !after
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown)
        );
    }

    #[test]
    fn retired_history_does_not_hide_a_late_current_folder_user() {
        let mut world = dudes_supervisor::World::default();
        for ordinal in 1..=520_u64 {
            let agent = AgentId::from_parts(1, u128::from(ordinal));
            fold(
                &mut world,
                ordinal * 2 - 1,
                hired(agent, Path::new("/old-folder")),
            );
            fold(
                &mut world,
                ordinal * 2,
                Record::AgentRetired {
                    id: agent,
                    reason: "finished".into(),
                    browser_retire: None,
                },
            );
        }
        let candidate = AgentId::from_parts(1, 521);
        fold(
            &mut world,
            1041,
            hired(candidate, Path::new("/current-folder")),
        );
        let census = current_recorded_census(&world, managed(candidate), None, None, &[]);
        assert_eq!(world.agents.len(), 521);
        assert_eq!(census.members.len(), 1);
        assert_eq!(census.members[0].agent, candidate);
        assert_eq!(census.members[0].cwd, Path::new("/current-folder"));
        assert!(!census.membership_unknown);
        assert_eq!(census.examined, 2);
    }

    #[test]
    fn frozen_paths_and_multiple_physical_modes_remain_in_the_union() {
        let agent = AgentId::from_parts(1, 1);
        let mut census = CurrentCensus::default();
        census.room().unwrap();
        census.member(
            agent,
            Some(Path::new("/current-folder")),
            Mode::Managed,
            None,
            false,
        );
        census.room().unwrap();
        let _ = census.snapshot(
            managed(agent),
            Some(&snapshot(Path::new("/frozen-folder"))),
            true,
        );
        census.room().unwrap();
        census.member(
            agent,
            Some(Path::new("/current-folder")),
            Mode::Attached,
            None,
            false,
        );
        assert_eq!(
            census
                .members
                .iter()
                .map(|member| (&member.cwd, member.mode))
                .collect::<Vec<_>>(),
            [
                (&PathBuf::from("/current-folder"), Mode::Managed),
                (&PathBuf::from("/frozen-folder"), Mode::Managed),
                (&PathBuf::from("/current-folder"), Mode::Attached),
            ]
        );
        assert_eq!(
            census.uncertain_folders.into_iter().collect::<Vec<_>>(),
            [
                PathBuf::from("/current-folder"),
                PathBuf::from("/frozen-folder")
            ]
        );
    }

    #[test]
    fn snapshotless_membership_is_unknown_and_the_candidate_survives_overflow() {
        let candidate = AgentId::from_parts(1, 2000);
        let mut known = CurrentCensus::default();
        let _ = known.snapshot(
            managed(candidate),
            Some(&snapshot(Path::new("/current-folder"))),
            false,
        );
        assert!(!known.membership_unknown);
        let _ = known.snapshot(managed(candidate), None, true);
        assert!(known.membership_unknown);

        let mut bounded = CurrentCensus::default();
        bounded.room().unwrap();
        bounded.member(
            candidate,
            Some(Path::new("/current-folder")),
            Mode::Managed,
            None,
            false,
        );
        for ordinal in 1..=511_u128 {
            bounded.room().unwrap();
            bounded.member(
                AgentId::from_parts(1, ordinal),
                Some(Path::new("/peer-folder")),
                Mode::Managed,
                None,
                false,
            );
        }
        assert!(bounded.room().is_err());
        assert!(bounded.membership_unknown);
        assert_eq!(bounded.members.len(), 512);
        assert_eq!(bounded.members[0].agent, candidate);
    }

    #[test]
    fn physical_receipts_need_the_complete_recorded_key_and_frozen_path() {
        let world = dudes_supervisor::World::default();
        let agent = AgentId::from_parts(1, 1);
        let path = Path::new("/frozen-folder");
        let key = AdoptionKey {
            pid: 4242,
            start_time: 7,
            boot: Some("recorded-boot".into()),
        };
        let mut census = CurrentCensus::default();
        census.native_snapshot(managed(agent), Some(&snapshot(path)), &key, false);
        census.physical_member(&world, agent, Some((&key, path)), Mode::Managed, false);
        assert!(!census.membership_unknown);
        let recycled = AdoptionKey {
            start_time: 8,
            ..key.clone()
        };
        census.physical_member(&world, agent, Some((&recycled, path)), Mode::Managed, false);
        assert!(census.membership_unknown);
        let mut missing_boot = CurrentCensus::default();
        missing_boot.native_snapshot(managed(agent), Some(&snapshot(path)), &key, false);
        let no_boot = AdoptionKey {
            boot: None,
            ..key.clone()
        };
        missing_boot.physical_member(&world, agent, Some((&no_boot, path)), Mode::Managed, false);
        assert!(missing_boot.membership_unknown);
        let mut moved = CurrentCensus::default();
        moved.native_snapshot(managed(agent), Some(&snapshot(path)), &key, false);
        moved.physical_member(
            &world,
            agent,
            Some((&key, Path::new("/different-folder"))),
            Mode::Managed,
            false,
        );
        assert!(moved.membership_unknown);
        assert_eq!(moved.members.len(), 2);
    }

    fn cleanup_unknown(owner: InstructionObservationOwner, started: Lsn) -> Record {
        Record::InstructionObservationFinished {
            owner,
            ticket: None,
            baseline: InstructionBaselineRef::Started { lsn: started },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::CleanupUnconfirmed,
                end: None,
                sampled_at: None,
                recovery: true,
                comparison: InstructionComparison {
                    gaps: vec![InstructionCoverageGap {
                        origin: dudes_proto::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::CleanupUnconfirmed,
                        context: InstructionPathContext::Unresolved,
                        agent: Some(owner_agent(owner)),
                        path: None,
                    }],
                    ..InstructionComparison::default()
                },
            },
        }
    }

    #[test]
    fn detached_finished_attached_receipts_keep_their_frozen_folder_in_the_census() {
        let mut world = dudes_supervisor::World::default();
        let peer = AgentId::from_parts(1, 1);
        let candidate = AgentId::from_parts(1, 2);
        fold(&mut world, 1, hired(peer, Path::new("/frozen-folder")));
        fold(
            &mut world,
            2,
            Record::Attached {
                agent: peer,
                by: dudes_proto::record::Actor::System,
                hook_port: None,
                provenance: None,
            },
        );
        let owner = InstructionObservationOwner::AttachedProcess {
            agent: peer,
            attachment_lsn: Lsn(2),
        };
        fold(
            &mut world,
            3,
            Record::InstructionObservationStarted {
                owner,
                ticket: None,
                snapshot: snapshot(Path::new("/frozen-folder")),
            },
        );
        fold(
            &mut world,
            4,
            Record::PtySpawned {
                agent: peer,
                pty: dudes_proto::rpc::pty::PtyId(1),
                pid: 4242,
                start_time: 7,
                boot: None,
            },
        );
        fold(
            &mut world,
            5,
            Record::Detached {
                agent: peer,
                by: dudes_proto::record::Actor::System,
                provenance: None,
            },
        );
        fold(
            &mut world,
            6,
            Record::AgentRetired {
                id: peer,
                reason: "finished".into(),
                browser_retire: None,
            },
        );
        fold(
            &mut world,
            7,
            hired(candidate, Path::new("/current-folder")),
        );
        fold(&mut world, 8, cleanup_unknown(owner, Lsn(3)));
        assert!(world.attached_native_spawn_receipt(Lsn(3)).is_some());
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .finished
                .is_some()
        );
        let census = current_recorded_census(&world, managed(candidate), None, None, &[]);
        assert!(!census.membership_unknown);
        assert_eq!(census.members[0].agent, candidate);
        assert!(census.members.iter().any(|member| member.agent == peer
            && member.cwd == Path::new("/frozen-folder")
            && member.mode == Mode::Attached));
        assert!(
            census
                .uncertain_folders
                .contains(Path::new("/frozen-folder"))
        );
    }

    #[test]
    fn retired_open_legacy_turns_are_current_members_with_unknown_coverage() {
        let mut world = dudes_supervisor::World::default();
        let peer = AgentId::from_parts(1, 1);
        let candidate = AgentId::from_parts(1, 2);
        fold(&mut world, 1, hired(peer, Path::new("/legacy-folder")));
        fold(
            &mut world,
            2,
            Record::TurnIssued {
                agent: peer,
                turn: TurnId::from_parts(1, 1),
                goal: dudes_types::GoalId::from_parts(1, 1),
                task: "run the tests".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn(1),
                answers: Vec::new(),
            },
        );
        fold(
            &mut world,
            3,
            Record::AgentRetired {
                id: peer,
                reason: "finished".into(),
                browser_retire: None,
            },
        );
        fold(
            &mut world,
            4,
            hired(candidate, Path::new("/current-folder")),
        );
        assert_eq!(world.open_turn_owner_ids().len(), 1);
        let census = current_recorded_census(&world, managed(candidate), None, None, &[]);
        assert!(census.membership_unknown);
        assert_eq!(census.members[0].agent, candidate);
        assert!(
            census
                .members
                .iter()
                .any(|member| member.agent == peer && member.cwd == Path::new("/legacy-folder"))
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn exact_other_folder_uncertainty_does_not_certify_or_block_this_folder() {
        let candidate = tempfile::tempdir().unwrap();
        let peer = tempfile::tempdir().unwrap();
        let candidate_path = canonical(candidate.path()).unwrap();
        let deadline = Instant::now() + SAMPLE_DEADLINE;
        assert!(!scoped_membership_unknown(
            false,
            &[peer.path().to_path_buf()],
            &candidate_path,
            deadline
        ));
        assert!(scoped_membership_unknown(
            false,
            &[candidate.path().to_path_buf()],
            &candidate_path,
            deadline
        ));
        assert!(scoped_membership_unknown(
            false,
            &[peer.path().join("missing")],
            &candidate_path,
            deadline
        ));
        assert!(scoped_membership_unknown(
            true,
            &[peer.path().to_path_buf()],
            &candidate_path,
            deadline
        ));
    }
}
