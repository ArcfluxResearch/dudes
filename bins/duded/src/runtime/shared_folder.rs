use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use dudes_supervisor::host::{OwnedTargetState, OwnedTargets, OwnedTargetsStatus};
use dudes_supervisor::{
    AdoptionKey, Clock, Liveness, ProcessHost, Rng, SharedFolderRefusal, Supervisor,
};
use dudes_types::{AgentId, IsolationMode, Mode, Provider};

use super::{Ctx, Shared};
use crate::predicate::PredicateRunner;

const MAX_CHILDREN: usize = 512;

pub(super) fn canonical_folder(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    std::fs::metadata(&canonical)
        .ok()?
        .is_dir()
        .then_some(canonical)
}

struct Census<'a, H> {
    host: &'a H,
    world: &'a dudes_supervisor::World,
    candidate: Option<AgentId>,
    recovery_consent: Option<&'a dudes_proto::recovery::RecoveryBinding>,
    folder: PathBuf,
    examined: usize,
    conflict: Option<(AgentId, String)>,
    unknown: Option<SharedFolderRefusal>,
}

impl<H: ProcessHost> Census<'_, H> {
    fn room(&mut self) -> Result<(), SharedFolderRefusal> {
        if self.examined == MAX_CHILDREN {
            self.unknown.get_or_insert(SharedFolderRefusal::ChildLimit);
            return Err(SharedFolderRefusal::ChildLimit);
        }
        self.examined += 1;
        Ok(())
    }

    fn child(&mut self, agent: AgentId, cwd: &Path, key: Option<&AdoptionKey>) {
        if Some(agent) == self.candidate {
            return;
        }
        let live = key.map_or(Liveness::Unknown, |key| self.host.alive(key));
        if self.scoped_owner(agent) {
            let Some(folders) = self.owner_folders(agent) else {
                self.unknown
                    .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                return;
            };
            for folder in folders {
                self.native_child(
                    agent,
                    &folder,
                    if live == Liveness::Alive {
                        OwnedTargetState::Present
                    } else {
                        OwnedTargetState::UnknownMissingCapability
                    },
                );
            }
            return;
        }
        if live == Liveness::Gone {
            return;
        }
        let Some(folder) = canonical_folder(cwd) else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerFolderUnresolved { agent });
            return;
        };
        if folder != self.folder {
            return;
        }
        if live == Liveness::Unknown {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerLivenessUnknown { agent });
        } else if let Some(row) = self.world.agents.get(&agent) {
            if self
                .conflict
                .as_ref()
                .is_none_or(|(other, _)| agent < *other)
            {
                let name = dudes_types::DisplayName::parse(&row.name).map_or_else(
                    |_| "agent".to_owned(),
                    dudes_types::DisplayName::into_string,
                );
                self.conflict = Some((agent, name));
            }
        } else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
        }
    }

    fn native_child(&mut self, agent: AgentId, cwd: &Path, state: OwnedTargetState) {
        if Some(agent) == self.candidate || state == OwnedTargetState::NativeBoundaryAbsent {
            return;
        }
        let Some(folder) = canonical_folder(cwd) else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerFolderUnresolved { agent });
            return;
        };
        if folder != self.folder {
            return;
        }
        if state != OwnedTargetState::Present {
            self.unknown
                .get_or_insert(SharedFolderRefusal::PeerLivenessUnknown { agent });
            return;
        }
        let Some(row) = self.world.agents.get(&agent) else {
            self.unknown
                .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
            return;
        };
        if self
            .conflict
            .as_ref()
            .is_none_or(|(other, _)| agent < *other)
        {
            let name = dudes_types::DisplayName::parse(&row.name).map_or_else(
                |_| "agent".to_owned(),
                dudes_types::DisplayName::into_string,
            );
            self.conflict = Some((agent, name));
        }
    }

    fn attempt_uncertain(&mut self, agent: AgentId, cwd: Option<&Path>) {
        match cwd.and_then(canonical_folder) {
            Some(folder) if folder != self.folder => {}
            Some(_) => {
                self.unknown
                    .get_or_insert(SharedFolderRefusal::PeerLivenessUnknown { agent });
            }
            None => {
                self.unknown
                    .get_or_insert(SharedFolderRefusal::PeerFolderUnresolved { agent });
            }
        }
    }

    fn scoped_owner(&self, agent: AgentId) -> bool {
        self.world.agents.get(&agent).is_some_and(|row| {
            matches!(
                row.repositories,
                dudes_proto::repository::HireRepositories::Scoped(_)
            )
        })
    }

    fn owner_folders(&self, agent: AgentId) -> Option<Vec<PathBuf>> {
        let row = self.world.agents.get(&agent)?;
        match &row.repositories {
            dudes_proto::repository::HireRepositories::Legacy(repository) => {
                let path = match repository.worktree() {
                    Some(lease) => self.world.worktrees.get(&lease)?.path.clone(),
                    None => repository.cwd().to_path_buf(),
                };
                Some(vec![path])
            }
            dudes_proto::repository::HireRepositories::Scoped(hire) => Some(
                hire.repositories
                    .entries()
                    .iter()
                    .map(|copy| copy.value.recorded.execution.directory.clone())
                    .collect(),
            ),
        }
    }

    fn recorded(&mut self, agent: AgentId, key: Option<&AdoptionKey>) {
        let Some(cwd) = self
            .owner_folders(agent)
            .and_then(|folders| folders.into_iter().next())
        else {
            if key.is_none_or(|key| self.host.alive(key) != Liveness::Gone) {
                self.unknown
                    .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
            }
            return;
        };
        self.child(agent, &cwd, key);
    }

    fn recorded_native_targets(
        &mut self,
        targets: &OwnedTargets,
        keys: &mut Vec<AdoptionKey>,
        turns: &mut BTreeSet<dudes_types::TurnId>,
    ) -> Result<(), SharedFolderRefusal> {
        let world = self.world;
        let receipts = world
            .managed_native_spawn_receipts()
            .map(|(_, spawn)| (spawn.owner, &spawn.key))
            .chain(
                world
                    .attached_native_spawn_receipts()
                    .map(|(_, spawn)| (spawn.owner, &spawn.key)),
            );
        for (owner, recorded_key) in receipts {
            if self.room().is_err() {
                return Err(SharedFolderRefusal::ChildLimit);
            }
            let owner = match owner {
                dudes_proto::InstructionObservationOwner::Managed { agent, turn, .. } => {
                    turns.insert(turn);
                    agent
                }
                dudes_proto::InstructionObservationOwner::AttachedProcess { agent, .. }
                | dudes_proto::InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
            };
            keys.push(recorded_key.clone());
            let mut matching = targets.targets.iter().filter(|target| {
                target
                    .key
                    .as_ref()
                    .is_some_and(|key| super::native_boundaries::same_key(key, recorded_key))
            });
            let target = matching.next();
            let mut state = if matching.next().is_some() {
                OwnedTargetState::UnknownIdentity
            } else {
                target.map_or(OwnedTargetState::UnknownMissingCapability, |target| {
                    if target.state == OwnedTargetState::NativeBoundaryAbsent
                        && target.native_absence_receipt.is_none()
                    {
                        OwnedTargetState::UnknownMissingCapability
                    } else {
                        target.state
                    }
                })
            };
            if state != OwnedTargetState::NativeBoundaryAbsent
                && self.host.alive(recorded_key) == Liveness::Alive
            {
                state = OwnedTargetState::Present;
            }
            let folders = if self.scoped_owner(owner) {
                self.owner_folders(owner)
            } else {
                target
                    .map(|target| vec![target.cwd.clone()])
                    .or_else(|| self.owner_folders(owner))
            };
            match folders {
                Some(folders) => {
                    for folder in folders {
                        self.native_child(owner, &folder, state);
                    }
                }
                None => {
                    self.unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
            }
        }
        Ok(())
    }

    fn recorded_attempts(
        &mut self,
        live_ptys: &std::collections::BTreeMap<AgentId, dudes_proto::rpc::pty::PtyId>,
    ) -> Result<(), SharedFolderRefusal> {
        let world = self.world;
        let retry = self.candidate.and_then(|agent| {
            super::native_boundaries::owed_retry_observation(
                world,
                agent,
                world.agents.get(&agent).and_then(|row| row.open_turn),
            )
        });
        for owner in world.instructions.unresolved_owners() {
            for (started, attempt) in world.instructions.unresolved_launch_attempts(owner) {
                if self.room().is_err() {
                    return Err(SharedFolderRefusal::ChildLimit);
                }
                let current_before_host = Some(owner) == self.candidate
                    && attempt.finished.is_none()
                    && attempt.spawn_lsn.is_none()
                    && match attempt.owner {
                        dudes_proto::InstructionObservationOwner::Managed { turn, .. } => world
                            .agents
                            .get(&owner)
                            .is_some_and(|row| row.open_turn == Some(turn)),
                        dudes_proto::InstructionObservationOwner::AttachedProcess { .. } => {
                            !live_ptys.contains_key(&owner)
                        }
                        dudes_proto::InstructionObservationOwner::AttachedTurn { .. } => false,
                    };
                let owed = super::native_boundaries::historical_owed_observation(
                    world, retry, *started, attempt,
                ) || self.recovery_consent.is_some_and(|binding| {
                    super::native_boundaries::original_owed_observation(
                        world, binding, *started, attempt,
                    )
                });
                if !current_before_host && !owed {
                    if let Some(evidence) = &attempt.snapshot {
                        for (_, snapshot) in evidence.samples() {
                            let folder = snapshot
                                .execution_folder
                                .as_ref()
                                .and_then(|path| path.to_path().ok());
                            self.attempt_uncertain(owner, folder.as_deref());
                        }
                    } else {
                        self.attempt_uncertain(owner, None);
                    }
                }
            }
        }
        Ok(())
    }

    fn result(self) -> Result<(), SharedFolderRefusal> {
        if let Some((agent, name)) = self.conflict {
            Err(SharedFolderRefusal::LivePeer { agent, name })
        } else if let Some(unknown) = self.unknown {
            Err(unknown)
        } else {
            Ok(())
        }
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn admit_shared_folder<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: Option<AgentId>,
        provider: Provider,
        requested: Option<IsolationMode>,
        mode: Mode,
        cwd: &Path,
    ) -> Result<(), SharedFolderRefusal> {
        if provider != Provider::Claude
            || (requested != Some(IsolationMode::Inherit)
                && !sup
                    .adapters()
                    .get(provider)
                    .is_some_and(|adapter| adapter.context_mode(mode).claude_inherit()))
        {
            return Ok(());
        }
        self.admit_folder_children(sup, agent, cwd)
    }

    fn admit_folder_children<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: Option<AgentId>,
        cwd: &Path,
    ) -> Result<(), SharedFolderRefusal> {
        self.admit_folder_children_for(sup, agent, cwd, None)
    }

    fn admit_folder_children_for<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: Option<AgentId>,
        cwd: &Path,
        recovery_consent: Option<&dudes_proto::recovery::RecoveryBinding>,
    ) -> Result<(), SharedFolderRefusal> {
        let folder = canonical_folder(cwd).ok_or(SharedFolderRefusal::CandidateFolderUnresolved)?;
        let world = sup.world();
        let mut census = Census {
            host: &self.host,
            world,
            candidate: agent,
            recovery_consent,
            folder,
            examined: 0,
            conflict: None,
            unknown: None,
        };
        let targets = self.host.owned_targets(MAX_CHILDREN);
        if census
            .candidate
            .is_some_and(|agent| census.scoped_owner(agent))
            && !matches!(targets.status, OwnedTargetsStatus::Complete)
        {
            census
                .unknown
                .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
        }
        if matches!(targets.status, OwnedTargetsStatus::Truncated { .. }) {
            census
                .unknown
                .get_or_insert(SharedFolderRefusal::ChildLimit);
        }
        let mut native_keys = Vec::new();
        let mut native_turns = BTreeSet::new();
        if census
            .recorded_native_targets(&targets, &mut native_keys, &mut native_turns)
            .is_err()
        {
            return census.result();
        }
        if census.recorded_attempts(&self.live_ptys).is_err() {
            return census.result();
        }
        let mut turns = native_turns;

        if self
            .admit_runtime_peers(&mut census, &mut turns, &mut native_keys)
            .is_err()
        {
            return census.result();
        }
        for target in &targets.targets {
            if (target.state == OwnedTargetState::NativeBoundaryAbsent
                && target.key.is_some()
                && target.native_absence_receipt.is_some())
                || target.key.as_ref().is_some_and(|key| {
                    native_keys
                        .iter()
                        .any(|seen| super::native_boundaries::same_key(key, seen))
                })
            {
                continue;
            }
            if census.room().is_err() {
                return census.result();
            }
            match canonical_folder(&target.cwd) {
                Some(folder) if folder != census.folder => {}
                _ => {
                    census
                        .unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
            }
        }
        if self.admit_remaining_peers(&mut census, &turns).is_err() {
            return census.result();
        }
        census.result()
    }
    pub(super) fn recovery_consent_checkout_clear<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: AgentId,
    ) -> bool {
        let turn = sup.world().agents.get(&agent).and_then(|row| row.open_turn);
        let owed = super::native_boundaries::missing_spawn_binding(sup, agent, turn);
        self.recovery_checkout_for(sup, agent, owed.as_ref())
    }

    pub(super) fn recovery_checkout_clear<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: AgentId,
    ) -> bool {
        self.recovery_checkout_for(sup, agent, None)
    }

    fn recovery_checkout_for<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        agent: AgentId,
        recovery_consent: Option<&dudes_proto::recovery::RecoveryBinding>,
    ) -> bool {
        let Some(row) = sup.world().agents.get(&agent) else {
            return false;
        };
        let folders = match &row.repositories {
            dudes_proto::repository::HireRepositories::Legacy(repository) => {
                let cwd = match repository.worktree() {
                    Some(lease) => match sup.world().worktrees.get(&lease) {
                        Some(lease) if lease.removed.is_none() => lease.path.as_path(),
                        _ => return false,
                    },
                    None => repository.cwd(),
                };
                vec![cwd]
            }
            dudes_proto::repository::HireRepositories::Scoped(hire) => {
                let mut folders = Vec::new();
                for entry in hire.repositories.entries() {
                    let copy = &entry.value.recorded;
                    if !crate::worktree::is_owned_repository(copy)
                        || sup
                            .world()
                            .worktrees
                            .get(&copy.worktree)
                            .is_none_or(|lease| {
                                lease.agent != Some(agent)
                                    || lease.removed.is_some()
                                    || lease.path != copy.execution.directory
                                    || lease.origin != copy.origin.directory
                                    || lease.base != copy.base
                            })
                    {
                        return false;
                    }
                    folders.push(copy.execution.directory.as_path());
                }
                folders
            }
        };
        folders.into_iter().all(|cwd| {
            let Some(folder) = canonical_folder(cwd) else {
                return false;
            };
            self.recovery_checks_clear(&folder)
                && self
                    .admit_folder_children_for(sup, Some(agent), &folder, recovery_consent)
                    .is_ok()
        })
    }

    fn recovery_checks_clear(&self, folder: &Path) -> bool {
        let active = self.baselines.active_folders(MAX_CHILDREN + 1).chain(
            self.predicate_folders
                .keys()
                .map(|(_, path)| Some(path.as_path()))
                .take(MAX_CHILDREN + 1),
        );
        for (index, active) in active.enumerate().take(MAX_CHILDREN + 1) {
            if index == MAX_CHILDREN || active.is_none_or(|active| active == folder) {
                return false;
            }
        }
        true
    }

    pub(super) fn recovery_turn_checkout_clear<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        turn: dudes_types::TurnId,
    ) -> bool {
        let Some(row) = sup.world().turns.get(&turn) else {
            return false;
        };
        let Some(goal) = row.goal else {
            return true;
        };
        let Ok((state, _)) = sup.world().recovery_projection() else {
            return false;
        };
        match state.work_claim(dudes_proto::recovery::RecoveryWork {
            agent: row.agent,
            goal,
            turn,
        }) {
            Ok(None) => true,
            Ok(Some(_)) => self.recovery_checkout_clear(sup, row.agent),
            Err(_) => false,
        }
    }

    fn admit_runtime_peers(
        &self,
        census: &mut Census<'_, H>,
        turns: &mut BTreeSet<dudes_types::TurnId>,
        native_keys: &mut Vec<AdoptionKey>,
    ) -> Result<(), SharedFolderRefusal> {
        let world = census.world;
        let current: Result<(), SharedFolderRefusal> =
            self.children.visit_owned(|turn, key, cwd| {
                if turns.contains(&turn) {
                    return Ok(());
                }
                census.room()?;
                turns.insert(turn);
                native_keys.push(key.clone());
                if let Some(row) = world.turns.get(&turn) {
                    match cwd {
                        Some(cwd) => census.child(row.agent, cwd, Some(&key)),
                        None => census.recorded(row.agent, Some(&key)),
                    }
                } else if self.host.alive(&key) != Liveness::Gone {
                    census
                        .unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
                Ok(())
            });
        if current.is_err() {
            return Err(SharedFolderRefusal::ChildLimit);
        }
        let uncertain: Result<bool, SharedFolderRefusal> =
            self.children.visit_uncertain(|turn, cwd| {
                census.room()?;
                if let Some(row) = world.turns.get(&turn) {
                    census.child(row.agent, cwd, None);
                } else {
                    census
                        .unknown
                        .get_or_insert(SharedFolderRefusal::ChildOwnerUnresolved);
                }
                Ok(())
            });
        if !matches!(uncertain, Ok(false)) {
            census
                .unknown
                .get_or_insert(SharedFolderRefusal::ChildLimit);
            return Err(SharedFolderRefusal::ChildLimit);
        }
        Ok(())
    }

    fn admit_remaining_peers(
        &self,
        census: &mut Census<'_, H>,
        turns: &BTreeSet<dudes_types::TurnId>,
    ) -> Result<(), SharedFolderRefusal> {
        let world = census.world;
        let mut attachments = BTreeSet::new();
        for (owner, pty) in &self.live_ptys {
            if census.room().is_err() {
                return Err(SharedFolderRefusal::ChildLimit);
            }
            if let Some((key, cwd)) = self.pty_host.receipt(*pty) {
                census.child(*owner, cwd, Some(key));
                attachments.insert(*owner);
            }
        }
        for (turn, row) in world.open_children() {
            if !turns.contains(&turn) {
                if census.room().is_err() {
                    return Err(SharedFolderRefusal::ChildLimit);
                }
                census.recorded(row.agent, row.adoption_key().as_ref());
            }
        }
        for (owner, key) in world.attached_children() {
            if !attachments.contains(&owner) {
                if census.room().is_err() {
                    return Err(SharedFolderRefusal::ChildLimit);
                }
                census.recorded(owner, Some(key));
                attachments.insert(owner);
            }
        }
        for owner in self
            .live_ptys
            .keys()
            .filter(|owner| !attachments.contains(owner))
        {
            census.recorded(*owner, None);
        }
        for cleanup in self.cleanup_ptys.values() {
            if !cleanup.pty_close_recorded {
                if census.room().is_err() {
                    return Err(SharedFolderRefusal::ChildLimit);
                }
                census.recorded(cleanup.agent, None);
            }
        }
        Ok(())
    }
}
