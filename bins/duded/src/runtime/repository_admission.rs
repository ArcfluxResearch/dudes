//! Current project enrollment and execution observations, separate from journal history.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, anyhow, ensure};
use dudes_proto::record::TrustedRepository;
use dudes_proto::repository::{
    CatalogRepository, Confinement, HireRepositories, RepoId, RepositoryEntry, RepositorySet,
};
use dudes_proto::{Entry, Record};
use dudes_supervisor::{Clock, ProcessHost, RepositoryAdmission, Rng, World};
use dudes_types::{AgentId, Lsn, Timestamp};

use super::{Ctx, Daemon, PredicateRunner};
use crate::floor_actions::{ConfiguredRepository, Enrollment, Profile};

#[derive(Clone, Default)]
pub(crate) struct CurrentRepositories {
    pub(crate) observed: BTreeMap<String, ObservedProject>,
    bindings: BTreeMap<String, BTreeMap<RepoId, TrustedRepository>>,
    #[cfg(test)]
    pub(crate) test_hire_capability: bool,
    #[cfg(test)]
    pub(crate) test_hire_policy: Option<dudes_proto::repository::RepositoryPolicy>,
}

impl CurrentRepositories {
    /// The policy a project hire records; only a test can select the confined one.
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "only a test can select a policy other than the fixed one"
        )
    )]
    pub(super) fn hire_policy(&self) -> dudes_proto::repository::RepositoryPolicy {
        #[cfg(test)]
        if let Some(policy) = self.test_hire_policy {
            return policy;
        }
        super::repository_hire::HIRE_POLICY
    }
}

#[derive(Clone)]
pub(crate) struct ObservedProject {
    pub(crate) lsn: Option<Lsn>,
    pub(crate) repositories: RepositorySet<CatalogRepository>,
}

fn catalog(
    profile: &Profile,
    recorded: Option<&dudes_proto::repository_state::history::ProjectRepositoryHistory>,
    bindings: Option<&BTreeMap<RepoId, TrustedRepository>>,
) -> Result<Option<RepositorySet<CatalogRepository>>> {
    let configured = if let Some(configured) = &profile.repositories {
        std::borrow::Cow::Borrowed(configured)
    } else {
        if crate::worktree::plain_git_dir(&profile.cwd).is_none() {
            ensure!(
                recorded.is_none() && bindings.is_none(),
                "an observed repository binding cannot be resolved"
            );
            return Ok(None);
        }
        let primary =
            dudes_proto::repository::RepoId::parse("primary").expect("static repository id");
        std::borrow::Cow::Owned(
            RepositorySet::new(
                primary.clone(),
                vec![RepositoryEntry {
                    repo_id: primary,
                    value: ConfiguredRepository {
                        label: profile.label.clone(),
                        path: profile.cwd.clone(),
                        written_path: profile.written_cwd.clone(),
                    },
                }],
            )
            .expect("one primary repository"),
        )
    };
    let entries = configured
        .entries()
        .iter()
        .map(|entry| {
            let directory = &entry.value.path;
            ensure!(
                directory.canonicalize().is_ok_and(|now| now == *directory),
                "repository location changed"
            );
            let common = crate::worktree::plain_git_dir(directory)
                .ok_or_else(|| anyhow!("repository is not a plain existing checkout"))?;
            Ok(RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: CatalogRepository {
                    label: entry.value.label.clone(),
                    origin: TrustedRepository {
                        directory: directory.clone(),
                        directory_identity: crate::worktree::trust::identity(directory)
                            .map_err(|error| anyhow!(error.to_string()))?,
                        common_identity: crate::worktree::trust::identity(&common)
                            .map_err(|error| anyhow!(error.to_string()))?,
                        common_dir: common,
                    },
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let repositories = RepositorySet::new(configured.primary_id().clone(), entries)?;
    if let Some(bindings) = bindings {
        for (repo_id, origin) in bindings {
            ensure!(
                repositories
                    .get(repo_id)
                    .is_none_or(|current| current.origin == *origin),
                "an observed repository id cannot be rebound"
            );
        }
    }
    for (index, member) in repositories.entries().iter().enumerate() {
        ensure!(
            !repositories.entries()[..index].iter().any(|other| {
                crate::worktree::contains(
                    &member.value.origin.directory,
                    &other.value.origin.directory,
                ) || crate::worktree::contains(
                    &other.value.origin.directory,
                    &member.value.origin.directory,
                ) || crate::worktree::same_directory(
                    &member.value.origin.common_dir,
                    &other.value.origin.common_dir,
                )
            }),
            "repository directories overlap or share Git metadata"
        );
    }
    dudes_proto::repository_state::history::validate_shape(&Record::ProjectRepositoriesRecorded {
        project: profile.id.clone(),
        repositories: repositories.clone(),
    })?;
    Ok(Some(repositories))
}

fn directory_overlaps(left: &Path, right: &Path) -> bool {
    directory_contains(left, right) || directory_contains(right, left)
}

fn directory_contains(ancestor: &Path, path: &Path) -> bool {
    path.starts_with(ancestor) || crate::worktree::contains(ancestor, path)
}

fn explicit_repository_roots<'a>(
    enrollment: &'a Enrollment,
    world: &'a World,
) -> impl Iterator<Item = &'a Path> {
    enrollment
        .profiles
        .iter()
        .filter_map(|profile| profile.repositories.as_ref())
        .flat_map(RepositorySet::entries)
        .map(|entry| entry.value.path.as_path())
        .chain(
            world
                .agents
                .values()
                .filter_map(|row| match &row.repositories {
                    HireRepositories::Scoped(scope) => Some(scope),
                    HireRepositories::Legacy(_) => None,
                })
                .flat_map(|scope| scope.repositories.entries())
                .flat_map(|entry| {
                    [
                        entry.value.recorded.execution.directory.as_path(),
                        entry.value.recorded.origin.directory.as_path(),
                    ]
                }),
        )
}

pub(super) struct CurrentHireAdmission<'a> {
    enrollment: &'a Enrollment,
    pub(super) current: &'a CurrentRepositories,
}

impl<'a> CurrentHireAdmission<'a> {
    pub(super) fn new(enrollment: &'a Enrollment, current: &'a CurrentRepositories) -> Self {
        Self {
            enrollment,
            current,
        }
    }

    pub(super) fn current_project_repositories(
        &self,
        world: &World,
        project: &str,
    ) -> Result<(&Profile, &ObservedProject)> {
        let profile = self
            .enrollment
            .profile(project)
            .filter(|profile| profile.repositories.is_some())
            .context("select a project with an explicit repository list")?;
        let current = catalog(
            profile,
            world.repository_projects.get(project),
            self.current.bindings.get(project),
        )?
        .context("current project repositories are unavailable")?;
        let observed = self
            .current
            .observed
            .get(project)
            .filter(|observed| observed.repositories == current)
            .context("current repository observation is unavailable")?;
        ensure!(
            observed.lsn.and_then(|lsn| world
                .repository_projects
                .get(project)?
                .revisions
                .get(&lsn))
                == Some(&observed.repositories),
            "repository enrollment is not available yet"
        );
        Ok((profile, observed))
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "tests may admit a provider capability through this admission instance"
        )
    )]
    pub(super) fn require_repository_capability(&self, confinement: Confinement) -> Result<()> {
        #[cfg(test)]
        if self.current.test_hire_capability {
            return Ok(());
        }
        match confinement {
            Confinement::Confined => anyhow::bail!(
                "restricted repository execution has no verified provider capability yet"
            ),
            Confinement::Unconfined => {
                anyhow::bail!("unconfined repository execution is not enabled yet")
            }
        }
    }

    pub(super) fn require_current_hire(
        &self,
        world: &World,
        cwd: &Path,
        at: Timestamp,
    ) -> Result<()> {
        ensure!(
            !explicit_repository_roots(self.enrollment, world)
                .any(|root| directory_overlaps(root, cwd)),
            "this folder belongs to an explicitly scoped project; repository execution is not available yet"
        );
        for origin in self.current.bindings.values().flat_map(BTreeMap::values) {
            if directory_overlaps(&origin.directory, cwd) {
                ensure!(
                    crate::worktree::trust::still_there(
                        origin,
                        &origin.directory,
                        &origin.directory
                    ) && crate::worktree::plain_git_dir(&origin.directory).as_ref()
                        == Some(&origin.common_dir),
                    "an observed repository binding cannot be resolved"
                );
            }
        }
        for profile in &self.enrollment.profiles {
            if directory_overlaps(&profile.cwd, cwd)
                && let Some(repositories) = catalog(
                    profile,
                    world.repository_projects.get(&profile.id),
                    self.current.bindings.get(&profile.id),
                )?
            {
                world.validate_repository_record(&Entry {
                    at,
                    record: Record::ProjectRepositoriesRecorded {
                        project: profile.id.clone(),
                        repositories,
                    },
                })?;
            }
        }
        Ok(())
    }

    pub(super) fn require_hire_authority(
        &self,
        world: &World,
        hire: dudes_proto::repository_state::HireRecord<'_>,
        at: Timestamp,
    ) -> Result<()> {
        match hire.repositories {
            dudes_proto::repository_state::HireRepositoryRecord::Legacy {
                cwd, worktree, ..
            } => {
                let origin = match worktree {
                    Some(id) => world
                        .worktrees
                        .get(&id)
                        .map(|lease| lease.origin.as_path())
                        .context("the hired worktree's origin is unavailable")?,
                    None => cwd,
                };
                self.require_current_hire(world, origin, at)
            }
            dudes_proto::repository_state::HireRepositoryRecord::Scoped(scope) => {
                self.require_scoped_hire_authority(world, scope)
            }
        }
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn project_repositories_current(&self, world: &World, profile: &Profile) -> bool {
        catalog(
            profile,
            world.repository_projects.get(&profile.id),
            self.current_repositories.bindings.get(&profile.id),
        )
        .and_then(|repositories| {
            if let Some(repositories) = repositories {
                world.validate_repository_record(&Entry {
                    at: self.clock.now(),
                    record: Record::ProjectRepositoriesRecorded {
                        project: profile.id.clone(),
                        repositories,
                    },
                })?;
            }
            Ok(())
        })
        .is_ok()
    }

    pub(super) fn require_current_hire(&self, world: &World, cwd: &Path) -> Result<()> {
        CurrentHireAdmission::new(&self.enrollment, &self.current_repositories)
            .require_current_hire(world, cwd, self.clock.now())
    }

    pub(super) fn require_hire_authority(
        &self,
        world: &World,
        hire: dudes_proto::repository_state::HireRecord<'_>,
    ) -> Result<()> {
        CurrentHireAdmission::new(&self.enrollment, &self.current_repositories)
            .require_hire_authority(world, hire, self.clock.now())
    }

    fn legacy_ticket_allowed(
        &self,
        world: &World,
        agent: AgentId,
        round: &dudes_proto::repository::TicketRepositoryRound,
    ) -> bool {
        let Some(repositories) = &round.repositories else {
            return round.authority.is_none();
        };
        if round.authority.is_some() || repositories.declared().is_some() {
            return false;
        }
        world
            .agents
            .get(&agent)
            .and_then(|row| row.repositories.workspace())
            .is_some_and(|hire| hire.id == repositories.project())
            && self
                .enrollment
                .profile(repositories.project())
                .is_some_and(|profile| profile.repositories.is_none())
    }

    pub(super) fn repository_allowed(&self, world: &World, agent: AgentId) -> bool {
        let Some(row) = world.agents.get(&agent) else {
            return false;
        };
        let repository = match &row.repositories {
            HireRepositories::Legacy(repository) => repository,
            HireRepositories::Scoped(scope) => {
                return row.provider == dudes_types::Provider::Claude
                    && CurrentHireAdmission::new(&self.enrollment, &self.current_repositories)
                        .require_scoped_hire_authority(world, &scope.recorded())
                        .is_ok();
            }
        };
        match world.active_repository_round(agent) {
            Ok(Some(round)) if !self.legacy_ticket_allowed(world, agent, round) => return false,
            Err(_) => return false,
            _ => {}
        }
        if row
            .open_turn
            .and_then(|turn| world.turns.get(&turn))
            .and_then(|turn| turn.repositories.as_ref())
            .and_then(|scope| scope.ticket())
            .is_some_and(|round| !self.legacy_ticket_allowed(world, agent, round))
        {
            return false;
        }
        let Some(project) = repository.workspace() else {
            let origin = repository
                .worktree()
                .and_then(|id| world.worktrees.get(&id))
                .map_or(repository.cwd(), |lease| lease.origin.as_path());
            return !self.explicit_repository_roots(world).any(|root| {
                directory_contains(root, repository.cwd()) || directory_contains(root, origin)
            });
        };
        let Some(profile) = self.enrollment.profile(&project.id) else {
            return false;
        };
        if profile.repositories.is_some() {
            return false;
        }
        if !self.project_repositories_current(world, profile) {
            return false;
        }
        if let Some(observed) = self.current_repositories.observed.get(&project.id) {
            let origin = &observed.repositories.primary().origin;
            if !crate::worktree::trust::still_there(origin, &profile.cwd, &profile.cwd)
                || crate::worktree::plain_git_dir(&profile.cwd).as_ref() != Some(&origin.common_dir)
            {
                return false;
            }
        }
        let origin = match repository.worktree() {
            Some(id) => match world.worktrees.get(&id) {
                Some(lease) if crate::worktree::is_worktree(repository.cwd(), &lease.origin) => {
                    &lease.origin
                }
                _ => return false,
            },
            None => repository.cwd(),
        };
        profile
            .cwd
            .canonicalize()
            .is_ok_and(|path| path == profile.cwd)
            && (origin == profile.cwd
                || (repository.worktree().is_some()
                    && crate::worktree::contains(origin, &profile.cwd)))
    }

    pub(super) fn repository_observation(&self, world: &World) -> RepositoryAdmission {
        self.repository_observation_with_gaps(world).0
    }

    /// The observation, and the unconfined agents that are not held, are refused execution and
    /// have a leased copy gone. The hold is recorded before a message, mail or ticket reaches
    /// such an agent, so no turn is issued into the gap and left without a process.
    pub(super) fn repository_observation_with_gaps(
        &self,
        world: &World,
    ) -> (RepositoryAdmission, Vec<(AgentId, dudes_types::WorktreeId)>) {
        let mut gaps = Vec::new();
        let observed: Vec<_> = world
            .agents
            .iter()
            .filter(|(_, row)| !row.retired)
            .map(|(agent, row)| {
                let allowed = self.repository_allowed(world, *agent);
                if !allowed
                    && row.automatic_hold.is_none()
                    && let Some(worktree) = super::missing_unconfined_copy(world, row)
                {
                    gaps.push((*agent, worktree));
                }
                (*agent, row.mcp.hire_lsn, allowed)
            })
            .collect();
        (RepositoryAdmission::new(observed), gaps)
    }

    pub(super) fn repository_turn_allowed(&self, world: &World, turn: dudes_types::TurnId) -> bool {
        world.turns.get(&turn).is_some_and(|row| {
            self.repository_allowed(world, row.agent)
                && row.repositories.as_ref().is_some_and(|scope| {
                    let Some(owner) = world.agents.get(&row.agent) else {
                        return false;
                    };
                    if !scope.matches_hire(owner.mcp.hire_lsn, &owner.repositories) {
                        return false;
                    }
                    if let Some(grant) = scope.grant() {
                        let dudes_proto::repository::HireRepositories::Scoped(hire) = scope.hire()
                        else {
                            return false;
                        };
                        if dudes_proto::repository::RepositoryTurnGrant::select(
                            scope.hired_lsn(),
                            hire,
                            scope.ticket(),
                        )
                        .as_ref()
                            != Ok(grant)
                        {
                            return false;
                        }
                        grant.ticket.as_ref().is_none_or(|round| {
                            world.ticket_repository_round(round.goal) == Some(round)
                                && round.authority.as_ref().is_some_and(|authority| {
                                    CurrentHireAdmission::new(
                                        &self.enrollment,
                                        &self.current_repositories,
                                    )
                                    .current_ticket_hire(world, row.agent, &authority.declaration)
                                    .is_ok()
                                })
                        })
                    } else {
                        scope
                            .ticket()
                            .is_none_or(|round| self.legacy_ticket_allowed(world, row.agent, round))
                    }
                })
        })
    }

    fn explicit_repository_roots<'a>(&'a self, world: &'a World) -> impl Iterator<Item = &'a Path> {
        explicit_repository_roots(&self.enrollment, world)
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn repository_trust_target(
        &self,
        project: &str,
        repo_id: &dudes_proto::repository::RepoId,
    ) -> Result<crate::worktree::hire::RepositoryTrustTarget> {
        let profile = self
            .ctx
            .enrollment
            .profile(project)
            .ok_or_else(|| anyhow!("project is not enrolled"))?;
        ensure!(
            profile.repositories.is_some(),
            "use singleton project trust without --repo"
        );
        let current = catalog(
            profile,
            self.sup.world().repository_projects.get(&profile.id),
            self.ctx.current_repositories.bindings.get(&profile.id),
        )?
        .ok_or_else(|| anyhow!("use singleton project trust without --repo"))?;
        let observed = self
            .ctx
            .current_repositories
            .observed
            .get(project)
            .filter(|observed| observed.repositories == current)
            .ok_or_else(|| anyhow!("current repository observation is unavailable"))?;
        let origin = current
            .get(repo_id)
            .ok_or_else(|| anyhow!("project has no repository with this id"))?
            .origin
            .clone();
        Ok(crate::worktree::hire::RepositoryTrustTarget {
            project: project.to_owned(),
            repo_id: repo_id.clone(),
            catalog_lsn: observed
                .lsn
                .ok_or_else(|| anyhow!("repository enrollment is not available yet"))?,
            origin,
        })
    }

    pub(super) fn set_repository_trust(
        &mut self,
        project: String,
        repo_id: dudes_proto::repository::RepoId,
        target: Option<&crate::worktree::hire::RepositoryTrustTarget>,
        vouch: Option<&crate::worktree::trust::Vouch>,
    ) -> Result<bool> {
        let (catalog_lsn, origin, pin) = match (target, vouch) {
            (Some(target), Some(vouch)) => {
                let now = self.repository_trust_target(&project, &repo_id)?;
                ensure!(
                    target.project == project
                        && target.repo_id == repo_id
                        && target.catalog_lsn == now.catalog_lsn
                        && target.origin == now.origin
                        && vouch.repository == now.origin
                        && crate::worktree::trust::still_there(
                            &now.origin,
                            &now.origin.directory,
                            &now.origin.directory
                        ),
                    "repository enrollment changed during trust inspection"
                );
                (now.catalog_lsn, now.origin, Some(vouch.pin.clone()))
            }
            (None, None) => {
                let Some(grant) = self
                    .sup
                    .world()
                    .repository_trust
                    .get(&(project.clone(), repo_id.clone()))
                    .filter(|grant| grant.pin.is_some())
                else {
                    return Ok(false);
                };
                (grant.catalog_lsn, grant.origin.clone(), None)
            }
            _ => return Err(anyhow!("repository trust inspection is incomplete")),
        };
        self.pump_or_refuse(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::SetRepositoryTrust {
                project,
                repo_id,
                catalog_lsn,
                origin,
                pin,
            },
        ))
        .map_err(|error| anyhow!(error.message))?;
        Ok(true)
    }

    pub(super) fn validate_repository_enrollment(&self, candidate: &Enrollment) -> Result<()> {
        for profile in &candidate.profiles {
            if let Some(repositories) = catalog(
                profile,
                self.sup.world().repository_projects.get(&profile.id),
                self.ctx.current_repositories.bindings.get(&profile.id),
            )? {
                self.sup.world().validate_repository_record(&Entry {
                    at: self.ctx.clock.now(),
                    record: Record::ProjectRepositoriesRecorded {
                        project: profile.id.clone(),
                        repositories,
                    },
                })?;
            }
        }
        for row in self.sup.world().agents.values().filter(|row| !row.retired) {
            if let Some(project) = row.repositories.workspace() {
                ensure!(
                    candidate.profile(&project.id).is_some(),
                    "a live hire still references the project"
                );
            }
            let HireRepositories::Legacy(repository) = &row.repositories else {
                continue;
            };
            let Some(project) = repository.workspace() else {
                continue;
            };
            let origin = repository
                .worktree()
                .and_then(|id| self.sup.world().worktrees.get(&id))
                .map_or(repository.cwd(), |lease| lease.origin.as_path());
            let retained = candidate.profile(&project.id).is_some_and(|profile| {
                profile
                    .repositories
                    .as_ref()
                    .map_or(profile.cwd == origin, |repositories| {
                        repositories
                            .entries()
                            .iter()
                            .any(|member| member.value.path == origin)
                    })
            });
            ensure!(
                retained,
                "a live hire still references the project repository"
            );
        }
        for (view, ticket) in self.ctx.projections.tickets()? {
            if view.status == dudes_types::CardStatus::Done {
                continue;
            }
            let project = ticket.repositories.project();
            let profile = candidate
                .profile(project)
                .ok_or_else(|| anyhow!("an unresolved ticket still references the project"))?;
            if ticket.repositories.declared().is_none()
                && let Some(history) = self.sup.world().repository_projects.get(project)
            {
                for origin in history.bindings.values() {
                    ensure!(
                        profile.repositories.as_ref().map_or(
                            profile.cwd == origin.directory,
                            |repositories| {
                                repositories
                                    .entries()
                                    .iter()
                                    .any(|member| member.value.path == origin.directory)
                            }
                        ),
                        "an unresolved singleton ticket cannot prove a removed repository is unreferenced"
                    );
                }
            }
        }
        for old in &self.ctx.enrollment.profiles {
            let next = candidate.profile(&old.id);
            let retains = next.is_some_and(|next| {
                next.repositories
                    .as_ref()
                    .map_or(next.cwd == old.cwd, |repos| {
                        repos
                            .entries()
                            .iter()
                            .any(|entry| entry.value.path == old.cwd)
                    })
            });
            if !retains {
                ensure!(
                    self.removal_blocker(&old.id, &old.cwd).is_none(),
                    "project is referenced by live work"
                );
            }
        }
        Ok(())
    }

    /// Append the catalog revision of every explicitly configured project that has none yet, then
    /// observe again so each observation carries its acknowledged position. A project configured by
    /// a plain folder is never recorded. The command is not idempotent, so an observation that
    /// already names a recorded revision is skipped.
    pub(super) fn record_missing_project_catalogs(&mut self) -> Result<()> {
        let missing: Vec<_> = self
            .ctx
            .enrollment
            .profiles
            .iter()
            .filter(|profile| profile.repositories.is_some())
            .filter_map(|profile| {
                let observed = self.ctx.current_repositories.observed.get(&profile.id)?;
                observed
                    .lsn
                    .is_none()
                    .then(|| (profile.id.clone(), observed.repositories.clone()))
            })
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        for (project, repositories) in missing {
            self.pump(dudes_supervisor::Input::Command(
                dudes_supervisor::Command::RecordProjectRepositories {
                    project,
                    repositories,
                },
            ))?;
        }
        self.observe_repository_enrollment()
    }

    pub(super) fn observe_repository_enrollment(&mut self) -> Result<()> {
        let profiles = self.ctx.enrollment.profiles.clone();
        let mut observed = BTreeMap::new();
        let mut failure = None;
        for profile in &profiles {
            let repositories = match catalog(
                profile,
                self.sup.world().repository_projects.get(&profile.id),
                self.ctx.current_repositories.bindings.get(&profile.id),
            )
            .and_then(|repositories| {
                if let Some(repositories) = &repositories {
                    self.sup.world().validate_repository_record(&Entry {
                        at: self.ctx.clock.now(),
                        record: Record::ProjectRepositoriesRecorded {
                            project: profile.id.clone(),
                            repositories: repositories.clone(),
                        },
                    })?;
                }
                Ok(repositories)
            }) {
                Ok(Some(repositories)) => repositories,
                Ok(None) => continue,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            let lsn = self
                .sup
                .world()
                .repository_projects
                .get(&profile.id)
                .and_then(|history| {
                    history
                        .revisions
                        .iter()
                        .find_map(|(lsn, saved)| (saved == &repositories).then_some(*lsn))
                });
            observed.insert(profile.id.clone(), ObservedProject { lsn, repositories });
        }
        for (project, observation) in &observed {
            let bindings = self
                .ctx
                .current_repositories
                .bindings
                .entry(project.clone())
                .or_default();
            for repository in observation.repositories.entries() {
                bindings.insert(repository.repo_id.clone(), repository.value.origin.clone());
            }
        }
        self.ctx.current_repositories.observed = observed;
        if let Some(failure) = failure {
            return Err(failure);
        }
        self.sup
            .observe_repository_admission(self.ctx.repository_observation(self.sup.world()));
        Ok(())
    }
}
