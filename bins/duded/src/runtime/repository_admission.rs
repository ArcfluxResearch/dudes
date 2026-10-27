//! Current project enrollment and execution observations, separate from journal history.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use dudes_proto::record::TrustedRepository;
use dudes_proto::repository::{
    CatalogRepository, HireRepositories, RepositoryEntry, RepositorySet,
};
use dudes_proto::{Entry, Record};
use dudes_supervisor::{Clock, ProcessHost, RepositoryAdmission, Rng, World};
use dudes_types::{AgentId, Lsn};

use super::{Ctx, Daemon, PredicateRunner};
use crate::floor_actions::{ConfiguredRepository, Enrollment, Profile};

#[derive(Default)]
pub(crate) struct CurrentRepositories {
    pub(crate) invalid: bool,
    pub(crate) invalid_seq: u64,
    pub(crate) observed: BTreeMap<String, ObservedProject>,
}

pub(crate) struct ObservedProject {
    pub(crate) lsn: Lsn,
    pub(crate) repositories: RepositorySet<CatalogRepository>,
}

fn catalog(profile: &Profile) -> Result<Option<RepositorySet<CatalogRepository>>> {
    let configured = if let Some(configured) = &profile.repositories {
        std::borrow::Cow::Borrowed(configured)
    } else {
        if crate::worktree::plain_git_dir(&profile.cwd).is_none() {
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

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    fn legacy_ticket_allowed(
        &self,
        world: &World,
        agent: AgentId,
        round: &dudes_proto::repository::TicketRepositoryRound,
    ) -> bool {
        let Some(repositories) = &round.repositories else {
            return round.authority.is_none();
        };
        if round.authority.is_some()
            || repositories.declared().is_some()
            || self.current_repositories.invalid
        {
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
        let HireRepositories::Legacy(repository) = &row.repositories else {
            return false;
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
            return true;
        };
        if self.current_repositories.invalid {
            return false;
        }
        let Some(profile) = self.enrollment.profile(&project.id) else {
            return false;
        };
        if profile.repositories.is_some() {
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
        RepositoryAdmission::new(world.agents.iter().filter(|(_, row)| !row.retired).map(
            |(agent, row)| {
                (
                    *agent,
                    row.mcp.hire_lsn,
                    self.repository_allowed(world, *agent),
                )
            },
        ))
    }

    pub(super) fn repository_turn_allowed(&self, world: &World, turn: dudes_types::TurnId) -> bool {
        world.turns.get(&turn).is_some_and(|row| {
            self.repository_allowed(world, row.agent)
                && row.repositories.as_ref().is_some_and(|scope| {
                    scope
                        .ticket()
                        .is_none_or(|round| self.legacy_ticket_allowed(world, row.agent, round))
                })
        })
    }

    pub(super) fn explicit_project_at(&self, world: &World, cwd: &Path) -> bool {
        self.enrollment
            .profiles
            .iter()
            .filter_map(|profile| profile.repositories.as_ref())
            .flat_map(RepositorySet::entries)
            .any(|entry| {
                crate::worktree::contains(&entry.value.path, cwd)
                    || crate::worktree::contains(cwd, &entry.value.path)
            })
            || world.agents.values().any(|row| {
                matches!(&row.repositories, HireRepositories::Scoped(scope)
                if scope.repositories.entries().iter().any(|entry| {
                    crate::worktree::contains(&entry.value.recorded.execution.directory, cwd)
                        || crate::worktree::contains(cwd, &entry.value.recorded.execution.directory)
                }))
            })
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn repository_trust_target(
        &self,
        project: &str,
        repo_id: &dudes_proto::repository::RepoId,
    ) -> Result<crate::worktree::hire::RepositoryTrustTarget> {
        ensure!(
            !self.ctx.current_repositories.invalid,
            "current project configuration is unavailable"
        );
        let profile = self
            .ctx
            .enrollment
            .profile(project)
            .ok_or_else(|| anyhow!("project is not enrolled"))?;
        ensure!(
            profile.repositories.is_some(),
            "use singleton project trust without --repo"
        );
        let current = catalog(profile)?
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
            catalog_lsn: observed.lsn,
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

    pub(super) fn invalidate_repository_enrollment(&mut self, seq: u64) -> Result<()> {
        if seq < self.ctx.reloads.local_seq() || seq < self.ctx.current_repositories.invalid_seq {
            return Ok(());
        }
        self.ctx.current_repositories.invalid = true;
        self.ctx.current_repositories.invalid_seq = seq;
        self.pump(dudes_supervisor::Input::Tick)
    }

    pub(super) fn validate_repository_enrollment(&self, candidate: &Enrollment) -> Result<()> {
        for profile in &candidate.profiles {
            if let Some(repositories) = catalog(profile)? {
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
            if view.status == dudes_types::CardStatus::Done
                || ticket.repositories.declared().is_some()
            {
                continue;
            }
            let project = ticket.repositories.project();
            let profile = candidate
                .profile(project)
                .ok_or_else(|| anyhow!("an unresolved ticket still references the project"))?;
            if let Some(history) = self.sup.world().repository_projects.get(project) {
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

    pub(super) fn observe_repository_enrollment(&mut self) -> Result<()> {
        self.validate_repository_enrollment(&self.ctx.enrollment)?;
        let profiles = self.ctx.enrollment.profiles.clone();
        let mut observed = BTreeMap::new();
        for profile in &profiles {
            let Some(repositories) = catalog(profile)? else {
                continue;
            };
            let known = self
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
            let lsn = if let Some(lsn) = known {
                lsn
            } else {
                self.pump_or_refuse(dudes_supervisor::Input::Command(
                    dudes_supervisor::Command::RecordProjectRepositories {
                        project: profile.id.clone(),
                        repositories: repositories.clone(),
                    },
                ))
                .map_err(|error| anyhow!(error.message))?;
                self.sup
                    .world()
                    .repository_projects
                    .get(&profile.id)
                    .and_then(|history| {
                        history
                            .revisions
                            .iter()
                            .find_map(|(lsn, saved)| (saved == &repositories).then_some(*lsn))
                    })
                    .ok_or_else(|| anyhow!("catalog was not observed"))?
            };
            observed.insert(profile.id.clone(), ObservedProject { lsn, repositories });
        }
        self.ctx.current_repositories.observed = observed;
        self.ctx.current_repositories.invalid = false;
        self.sup
            .observe_repository_admission(self.ctx.repository_observation(self.sup.world()));
        Ok(())
    }
}
