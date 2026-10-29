//! Loop-owned admission and atomic assignment for one complete repository group.

use dudes_proto::record::RecordedWorkspace;
use dudes_proto::repository::{
    RepoId, RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet, ScopedRepositories,
};
use dudes_proto::rpc::agent::{Checkout, HireParams, HireResult, HireTarget, ProjectHireTarget};
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_supervisor::{Clock, Command, Input, ProcessHost, Rng, World};
use dudes_types::{AgentId, GitCommit, IsolationMode, Provider};

use super::repository_admission::CurrentHireAdmission;
use super::{Ctx, Daemon, PredicateRunner, PreparedWorktreeLease};
use crate::ports::Answered;
use crate::worktree::hire::{PreparedRepositoryGroup, RepositoryHireOrigin, RepositoryHirePlan};

fn refused(message: impl Into<String>) -> RpcError {
    crate::worktree::unavailable(message)
}

fn require_scoped_options(params: &HireParams) -> anyhow::Result<()> {
    anyhow::ensure!(
        params.provider == Provider::Claude,
        "restricted repository execution has no verified provider capability for {}",
        params.provider
    );
    anyhow::ensure!(
        super::scoped_launch::native_platform()
            != dudes_providers::scoped::ScopedPlatform::Unsupported,
        "restricted repository execution has no verified provider capability on this platform"
    );
    anyhow::ensure!(
        params.isolation.is_none(),
        "restricted repository hires do not accept an isolation override"
    );
    anyhow::ensure!(
        params.model.as_ref().map(dudes_types::ModelName::as_str) == Some("claude-sonnet-4-6"),
        "restricted repository hires require the explicit claude-sonnet-4-6 model"
    );
    Ok(())
}

#[derive(Clone)]
pub(crate) struct RepositoryLeaseAppend {
    pub(super) plan: RepositoryHirePlan,
    pub(super) repo_id: RepoId,
    pub(super) prepared: PreparedWorktreeLease,
}

impl CurrentHireAdmission<'_> {
    fn current_repository_hire_target(
        &self,
        world: &World,
        project: &str,
    ) -> anyhow::Result<ProjectHireTarget> {
        let (_, observed) = self.current_project_repositories(world, project)?;
        Ok(ProjectHireTarget {
            project_id: project.to_owned(),
            expected_revision: observed.lsn.expect("recorded current catalog"),
        })
    }

    fn current_repository_hire_facts(
        &self,
        world: &World,
        target: &ProjectHireTarget,
    ) -> anyhow::Result<(RecordedWorkspace, RepositorySet<RepositoryHireOrigin>)> {
        let (profile, observed) = self.current_project_repositories(world, &target.project_id)?;
        anyhow::ensure!(
            observed.lsn == Some(target.expected_revision),
            "the project's observed repository revision changed"
        );
        anyhow::ensure!(
            profile.isolation == IsolationMode::Default,
            "restricted repository hires require default project isolation"
        );
        let catalog = &observed.repositories;
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

    pub(super) fn require_repository_hire_plan(
        &self,
        world: &World,
        plan: &RepositoryHirePlan,
    ) -> anyhow::Result<()> {
        require_scoped_options(&plan.params)?;
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

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn consume_repository_lease(
        &mut self,
        world: &World,
        entry: &dudes_proto::Entry,
    ) -> anyhow::Result<Option<RepositoryLeaseAppend>> {
        let Some(admitted) = self.repository_lease_append.take() else {
            return Ok(None);
        };
        let lease = entry.record.worktree_lease().ok_or_else(|| {
            anyhow::anyhow!("a repository lease admission cannot append another record")
        })?;
        CurrentHireAdmission::new(&self.enrollment, &self.current_repositories)
            .require_repository_hire_plan(world, &admitted.plan)?;
        let selected = admitted
            .plan
            .repositories
            .get(&admitted.repo_id)
            .ok_or_else(|| anyhow::anyhow!("the repository is outside the group admission"))?;
        let expected = &admitted.prepared.lease;
        anyhow::ensure!(
            lease.origin == selected.origin.directory
                && lease.worktree == expected.worktree
                && lease.origin == expected.origin
                && lease.base == expected.base
                && lease.path == expected.path
                && lease.branch == expected.branch
                && lease.from == admitted.prepared.from
                && lease.from.is_none()
                && lease.identity.is_some()
                && lease.identity == expected.identity.as_ref()
                && !world.worktrees.contains_key(&lease.worktree),
            "the repository lease does not match its exact admitted group member"
        );
        Ok(Some(admitted))
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn repository_hire_target(
        &self,
        project: &str,
    ) -> Result<ProjectHireTarget, RpcError> {
        CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories)
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
        require_scoped_options(&params).map_err(|error| refused(error.to_string()))?;
        if !params.mcp_servers.is_empty() {
            return Err(refused(
                "restricted repository hires cannot enable MCP servers",
            ));
        }
        self.hire_option_guards(&params)?;
        let (project, repositories) =
            CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories)
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
        CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories)
            .require_repository_hire_plan(self.sup.world(), &plan)
            .map_err(|error| refused(error.to_string()))?;
        Ok(plan)
    }

    fn revalidate_repository_hire(&self, plan: &RepositoryHirePlan) -> Result<(), RpcError> {
        self.hire_option_guards(&plan.params)?;
        CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories)
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
        if self.ctx.repository_lease_append.take().is_some() {
            return Ok(Answered::RepositoryHireRejected(refused(
                "another repository lease append is already admitted",
            )));
        }
        let prepared = (|| {
            self.revalidate_repository_hire(plan)?;
            let origin = plan
                .repositories
                .get(repo_id)
                .ok_or_else(|| refused("the repository is outside the original project admission"))?
                .origin
                .directory
                .clone();
            let prepared = self.prepare_worktree_lease(origin, base, None)?;
            self.revalidate_repository_hire(plan)?;
            Ok::<_, RpcError>(prepared)
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Answered::RepositoryHireRejected(error)),
        };
        self.ctx.repository_lease_append = Some(RepositoryLeaseAppend {
            plan: plan.clone(),
            repo_id: repo_id.clone(),
            prepared: prepared.clone(),
        });
        let appended = self.append_worktree_lease(prepared);
        self.ctx.repository_lease_append = None;
        match appended {
            Ok(lease) => Ok(Answered::WorktreeLeased(lease)),
            Err(failure) if super::known_preappend_refusal(&failure) => Ok(
                Answered::RepositoryHireRejected(refused(failure.to_string())),
            ),
            Err(failure) => Err(refused(format!(
                "the repository lease is uncertain; copies are retained for replay: {failure}"
            ))),
        }
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
                        || lease.identity.is_none()
                        || crate::worktree::pending::verify(&lease).is_err()
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
        prepared: &PreparedRepositoryGroup,
    ) -> Result<Answered, RpcError> {
        let hire = match self.prepared_repository_hire(prepared) {
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
            if super::known_preappend_refusal(&failure) && self.newest_agent() == before {
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
