//! Loop-owned admission and atomic assignment for one complete repository group.

use dudes_proto::record::RecordedWorkspace;
use dudes_proto::repository::{
    RepoId, RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet, ScopedRepositories,
};
use dudes_proto::rpc::agent::{Checkout, HireParams, HireResult, HireTarget, ProjectHireTarget};
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_supervisor::{Clock, Command, Input, ProcessHost, Rng, World};
use dudes_types::{AgentId, GitCommit, IsolationMode};

use super::{Ctx, Daemon, PredicateRunner};
use crate::ports::Answered;
use crate::worktree::hire::{PreparedRepositoryGroup, RepositoryHireOrigin, RepositoryHirePlan};

fn refused(message: impl Into<String>) -> RpcError {
    crate::worktree::unavailable(message)
}

pub(super) struct RepositoryLeaseAppend {
    pub(super) plan: RepositoryHirePlan,
    pub(super) repo_id: RepoId,
    pub(super) base: GitCommit,
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    fn current_repository_hire_target(
        &self,
        _world: &World,
        project: &str,
    ) -> anyhow::Result<ProjectHireTarget> {
        anyhow::ensure!(
            !self.current_repositories.invalid,
            "current project configuration is unavailable"
        );
        let profile = self
            .enrollment
            .profile(project)
            .filter(|profile| profile.repositories.is_some())
            .ok_or_else(|| anyhow::anyhow!("select a project with an explicit repository list"))?;
        let current = super::repository_admission::catalog(profile)?
            .ok_or_else(|| anyhow::anyhow!("current project repositories are unavailable"))?;
        let observed = self
            .current_repositories
            .observed
            .get(project)
            .filter(|observed| observed.repositories == current)
            .ok_or_else(|| anyhow::anyhow!("current repository observation is unavailable"))?;
        Ok(ProjectHireTarget {
            project_id: project.to_owned(),
            expected_revision: observed.lsn,
        })
    }

    fn current_repository_hire_facts(
        &self,
        world: &World,
        target: &ProjectHireTarget,
    ) -> anyhow::Result<(RecordedWorkspace, RepositorySet<RepositoryHireOrigin>)> {
        anyhow::ensure!(
            self.current_repository_hire_target(world, &target.project_id)? == *target,
            "the project's observed repository revision changed"
        );
        let profile = self
            .enrollment
            .profile(&target.project_id)
            .expect("current project target");
        anyhow::ensure!(
            profile.isolation != IsolationMode::Inherit,
            "restricted repository hires cannot inherit settings"
        );
        let catalog = &self.current_repositories.observed[&target.project_id].repositories;
        let entries = catalog
            .entries()
            .iter()
            .map(|entry| {
                let grant = world
                    .repository_trust
                    .get(&(target.project_id.clone(), entry.repo_id.clone()))
                    .filter(|grant| grant.origin == entry.value.origin)
                    .ok_or_else(|| {
                        anyhow::anyhow!("every current repository needs its own valid trust grant")
                    })?;
                let pin = grant.pin.clone().ok_or_else(|| {
                    anyhow::anyhow!("every current repository needs its own valid trust grant")
                })?;
                Ok(RepositoryEntry {
                    repo_id: entry.repo_id.clone(),
                    value: RepositoryHireOrigin {
                        origin: grant.origin.clone(),
                        trust_lsn: grant.lsn,
                        pin,
                    },
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((
            RecordedWorkspace {
                id: profile.id.clone(),
                label: profile.label.clone(),
            },
            RepositorySet::new(catalog.primary_id().clone(), entries)?,
        ))
    }

    fn require_repository_capability(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.current_repositories.invalid,
            "current project configuration is unavailable"
        );
        #[cfg(test)]
        if self.current_repositories.test_hire_capability {
            return Ok(());
        }
        anyhow::bail!("restricted repository execution has no verified provider capability yet");
    }

    fn require_repository_hire_plan(
        &self,
        world: &World,
        plan: &RepositoryHirePlan,
    ) -> anyhow::Result<()> {
        let target = plan
            .params
            .project
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("a project target is required"))?;
        let (project, repositories) = self.current_repository_hire_facts(world, target)?;
        anyhow::ensure!(
            plan.catalog_lsn == target.expected_revision
                && plan.project == project
                && plan.repositories == repositories,
            "the project's configuration or original trust grants changed during the copy"
        );
        self.require_repository_capability()
    }

    pub(super) fn consume_repository_lease(
        &mut self,
        world: &World,
        entry: &dudes_proto::Entry,
    ) -> anyhow::Result<bool> {
        let Some(admitted) = self.repository_lease_append.take() else {
            return Ok(false);
        };
        let dudes_proto::Record::WorktreeLeased {
            worktree,
            origin,
            base,
            from,
            path,
            branch,
        } = &entry.record
        else {
            anyhow::bail!("a repository lease admission cannot append another record");
        };
        self.require_repository_hire_plan(world, &admitted.plan)?;
        let selected = admitted
            .plan
            .repositories
            .get(&admitted.repo_id)
            .ok_or_else(|| anyhow::anyhow!("the repository is outside the group admission"))?;
        anyhow::ensure!(
            *origin == selected.origin.directory
                && *base == admitted.base
                && from.is_none()
                && !world.worktrees.contains_key(worktree)
                && *branch == format!("dudes/{}", worktree.short())
                && self
                    .agents_root
                    .parent()
                    .is_some_and(|root| *path == root.join("worktrees").join(worktree.to_string())),
            "the repository lease does not match its exact admitted group member"
        );
        Ok(true)
    }

    pub(super) fn require_scoped_hire_authority(
        &self,
        world: &World,
        scope: &ScopedRepositories,
    ) -> anyhow::Result<()> {
        let target = ProjectHireTarget {
            project_id: scope.project.id.clone(),
            expected_revision: scope.catalog_lsn,
        };
        let (project, originals) = self.current_repository_hire_facts(world, &target)?;
        anyhow::ensure!(
            scope.project == project
                && scope.policy == RepositoryPolicy::WorkingTreeV1
                && scope.repositories.primary_id() == originals.primary_id()
                && scope.repositories.entries().len() == originals.entries().len(),
            "the complete current repository authority is required"
        );
        for (copy, original) in scope.repositories.entries().iter().zip(originals.entries()) {
            anyhow::ensure!(
                copy.repo_id == original.repo_id
                    && copy.value.origin == original.value.origin
                    && copy.value.trust_lsn == original.value.trust_lsn
                    && crate::worktree::is_owned_repository(&copy.value),
                "a repository copy or original trust grant changed before the hire append"
            );
        }
        self.require_repository_capability()
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn repository_hire_target(
        &self,
        project: &str,
    ) -> Result<ProjectHireTarget, RpcError> {
        self.ctx
            .current_repository_hire_target(self.sup.world(), project)
            .map_err(|error| refused(error.to_string()))
    }

    pub(super) fn repository_hire_plan(
        &self,
        params: HireParams,
    ) -> Result<RepositoryHirePlan, RpcError> {
        let target = match params
            .target()
            .map_err(|message| dudes_rpc::dispatch::error(ErrorCode::InvalidParams, message))?
        {
            HireTarget::Project(target) => target,
            HireTarget::Directory(_) => return Err(refused("a project target is required")),
        };
        if params.checkout != Some(Checkout::Worktree) || params.from.is_some() || !params.trusted {
            return Err(refused(
                "a project hire requires trusted own copies of every repository, without from",
            ));
        }
        if params.isolation == Some(IsolationMode::Inherit) || !params.mcp_servers.is_empty() {
            return Err(refused(
                "restricted repository hires cannot inherit settings or enable MCP servers",
            ));
        }
        self.hire_option_guards(&params)?;
        let (project, repositories) = self
            .ctx
            .current_repository_hire_facts(self.sup.world(), target)
            .map_err(|error| refused(error.to_string()))?;
        for entry in repositories.entries() {
            self.refuse_overlap(&entry.value.origin.directory, None)?;
        }
        let plan = RepositoryHirePlan {
            project,
            catalog_lsn: target.expected_revision,
            repositories,
            params,
        };
        self.ctx
            .require_repository_hire_plan(self.sup.world(), &plan)
            .map_err(|error| refused(error.to_string()))?;
        Ok(plan)
    }

    fn revalidate_repository_hire(&self, plan: &RepositoryHirePlan) -> Result<(), RpcError> {
        self.hire_option_guards(&plan.params)?;
        self.ctx
            .require_repository_hire_plan(self.sup.world(), plan)
            .map_err(|error| refused(error.to_string()))?;
        for entry in plan.repositories.entries() {
            self.refuse_overlap(&entry.value.origin.directory, None)?;
        }
        Ok(())
    }

    pub(super) fn repository_hire_lease(
        &mut self,
        plan: &RepositoryHirePlan,
        repo_id: &RepoId,
        base: GitCommit,
    ) -> Result<Answered, RpcError> {
        self.revalidate_repository_hire(plan)?;
        let origin = plan
            .repositories
            .get(repo_id)
            .ok_or_else(|| refused("the repository is outside the original project admission"))?
            .origin
            .directory
            .clone();
        let before: std::collections::BTreeSet<_> =
            self.sup.world().worktrees.keys().copied().collect();
        if self.ctx.repository_lease_append.is_some() {
            return Err(refused(
                "another repository lease append is already admitted",
            ));
        }
        self.ctx.repository_lease_append = Some(RepositoryLeaseAppend {
            plan: plan.clone(),
            repo_id: repo_id.clone(),
            base,
        });
        let appended = self.pump_or_refuse(Input::Command(Command::LeaseWorktree {
            origin,
            base,
            from: None,
        }));
        self.ctx.repository_lease_append = None;
        appended?;
        self.sup
            .world()
            .worktrees
            .iter()
            .find(|(id, _)| !before.contains(*id))
            .map(|(id, row)| Answered::WorktreeLeased(super::lease_view(*id, row)))
            .ok_or_else(|| refused("the repository lease was not journaled"))
    }

    fn prepared_repository_hire(
        &self,
        prepared: &PreparedRepositoryGroup,
    ) -> Result<RepositoryHire, RpcError> {
        let plan = &prepared.plan;
        self.revalidate_repository_hire(plan)?;
        if prepared.repositories.primary_id() != plan.repositories.primary_id()
            || prepared.repositories.entries().len() != plan.repositories.entries().len()
        {
            return Err(refused("the complete repository copy set is required"));
        }
        for (copy, original) in prepared
            .repositories
            .entries()
            .iter()
            .zip(plan.repositories.entries())
        {
            let repo = &copy.value;
            if copy.repo_id != original.repo_id
                || repo.origin != original.value.origin
                || repo.trust_lsn != original.value.trust_lsn
                || !crate::worktree::is_owned_repository(repo)
                || self.pending_lease(repo.worktree).is_none_or(|lease| {
                    lease.origin != repo.origin.directory
                        || lease.path != repo.execution.directory
                        || lease.base != repo.base
                })
            {
                return Err(refused(
                    "a repository copy no longer matches its original lease and identities",
                ));
            }
            self.refuse_overlap(&repo.execution.directory, Some(repo.worktree))?;
        }
        let params = &plan.params;
        let hire = RepositoryHire {
            id: AgentId::NIL,
            name: dudes_types::DisplayName::parse(&params.name)
                .map_err(|error| refused(error.to_string()))?
                .into_string(),
            character: params.character,
            accent: params.accent,
            role: params.role.clone(),
            department: params.department.clone(),
            provider: params.provider,
            budget: params.budget,
            max_turns: params.max_turns,
            max_budget: params.max_budget,
            model: params.model.clone(),
            repositories: ScopedRepositories {
                project: plan.project.clone(),
                catalog_lsn: plan.catalog_lsn,
                policy: RepositoryPolicy::WorkingTreeV1,
                repositories: prepared.repositories.clone(),
            },
        };
        self.sup
            .world()
            .validate_repository_record(&dudes_proto::Entry {
                at: self.ctx.clock.now(),
                record: dudes_proto::Record::AgentHiredWithRepositories(Box::new(hire.clone())),
            })
            .map_err(|error| refused(error.to_string()))?;
        Ok(hire)
    }

    pub(super) fn repository_hire_commit(
        &mut self,
        prepared: PreparedRepositoryGroup,
    ) -> Result<Answered, RpcError> {
        let hire = match self.prepared_repository_hire(&prepared) {
            Ok(hire) => hire,
            Err(error) => return Ok(Answered::RepositoryHireRejected(error)),
        };
        let before = self.newest_agent();
        if let Err(failure) = self.pump(Input::Command(Command::HireWithRepositories(Box::new(
            hire,
        )))) {
            let error = dudes_rpc::dispatch::error(
                ErrorCode::InternalError,
                format!("the daemon could not complete the repository hire: {failure}"),
            );
            if failure.downcast_ref::<super::KnownNoEffect>().is_some()
                && self.newest_agent() == before
            {
                return Ok(Answered::RepositoryHireRejected(error));
            }
            return Err(error);
        }
        match self.newest_agent() {
            Some(agent) if Some(agent) != before => Ok(Answered::Hired(HireResult { agent })),
            _ => Err(refused(
                "the repository hire was not acknowledged; copies are retained for replay",
            )),
        }
    }
}
