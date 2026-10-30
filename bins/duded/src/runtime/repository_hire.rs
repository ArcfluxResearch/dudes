//! Loop-owned admission and atomic assignment for one complete repository group.

use dudes_proto::record::RecordedWorkspace;
use dudes_proto::repository::{
    Confinement, RepoId, RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet,
    ScopedRepositories,
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

/// The one policy the daemon records for a project hire. The confined policy is reachable only
/// through a test hook.
pub(super) const HIRE_POLICY: RepositoryPolicy = RepositoryPolicy::OperatorTrustV1;

/// The one engine and platform decision for a multi-repository hire. The hire path and the Floor
/// offer both ask it, so the two cannot disagree. An unconfined hire needs what a single-repo hire
/// into its own copy needs: Claude, on a platform that makes copies.
pub(super) fn group_engine_refusal(
    provider: Provider,
    confinement: Confinement,
) -> Option<&'static str> {
    if provider != Provider::Claude {
        return Some(match confinement {
            Confinement::Confined => {
                "Multi-repository projects are not available for this engine yet."
            }
            Confinement::Unconfined => "Multi-repository projects work with Claude only for now.",
        });
    }
    (super::scoped_launch::native_platform()
        == dudes_providers::scoped::ScopedPlatform::Unsupported)
        .then_some("Multi-repository hiring is not available on this platform.")
}

/// What a multi-repository hire needs before any lease. The unconfined policy takes the model and
/// the other options exactly as a single-repo hire into its own copy does, through
/// `hire_option_guards`; only the confined policy pins them.
pub(super) fn require_group_options(
    admission: &CurrentHireAdmission<'_>,
    params: &HireParams,
) -> anyhow::Result<()> {
    let confinement = admission.current.hire_policy().confinement();
    if matches!(confinement, Confinement::Unconfined) {
        admission.require_repository_capability(confinement)?;
    }
    if let Some(refusal) = group_engine_refusal(params.provider, confinement) {
        anyhow::bail!("{refusal}");
    }
    match confinement {
        Confinement::Confined => {
            anyhow::ensure!(
                params.isolation.is_none(),
                "restricted repository hires do not accept an isolation override"
            );
            anyhow::ensure!(
                params.model.as_ref().map(dudes_types::ModelName::as_str)
                    == Some("claude-sonnet-4-6"),
                "restricted repository hires require the explicit claude-sonnet-4-6 model"
            );
        }
        Confinement::Unconfined => {}
    }
    Ok(())
}

/// Refuse an unconfined hire whose task could not name every copy, before any lease exists. A
/// copy's folder is `<data>/worktrees/<lease id>`, and every lease id has the same length.
fn require_section_fits<'a>(
    data_dir: &std::path::Path,
    ids: impl IntoIterator<Item = &'a RepoId>,
) -> Result<(), dudes_proto::task::RepositorySectionError> {
    let root = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf())
        .join("worktrees");
    let planned = root.join("0".repeat(32));
    dudes_proto::task::repository_section(ids.into_iter().map(|id| (id, planned.as_path())))
        .map(drop)
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
        require_group_options(self, &plan.params)?;
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
        self.require_repository_capability(self.current.hire_policy().confinement())
    }

    pub(super) fn require_scoped_hire_authority(
        &self,
        world: &World,
        scope: &ScopedRepositories,
    ) -> anyhow::Result<()> {
        let confinement = scope.policy.confinement();
        let target = ProjectHireTarget {
            project_id: scope.project.id.clone(),
            expected_revision: scope.catalog_lsn,
        };
        let (project, originals) = self.current_repository_hire_facts(world, &target)?;
        anyhow::ensure!(
            scope.project == project
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
        self.require_repository_capability(confinement)
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
    pub(super) fn browser_repository_offer(
        &self,
        profile: &crate::floor_actions::Profile,
    ) -> Option<crate::floor_actions::RepositoryGroupOffer> {
        use crate::floor_actions::{
            OnboardProject, RepositoryGroupOffer, RepositoryLabel, RepositoryProviderOffer,
        };
        let admission =
            CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories);
        let (_, observed) = admission
            .current_project_repositories(self.sup.world(), &profile.id)
            .ok()?;
        let target = ProjectHireTarget {
            project_id: profile.id.clone(),
            expected_revision: observed.lsn?,
        };
        let trusted = admission
            .current_repository_hire_facts(self.sup.world(), &target)
            .is_ok();
        let available = self.offered_engines(profile);
        let confinement = self.ctx.current_repositories.hire_policy().confinement();
        let gate_open = admission.require_repository_capability(confinement).is_ok();
        let providers = crate::floor_actions::BROWSER_ENGINES
            .into_iter()
            .map(|provider| {
                let refusal = if !gate_open {
                    Some("Multi-repository hiring is not enabled yet.")
                } else if let Some(refusal) = group_engine_refusal(provider, confinement) {
                    Some(refusal)
                } else if !trusted {
                    Some("Trust every repository in this project before hiring.")
                } else if !self.onboarding_profile_resources_eligible(profile)
                    || !available.contains(&provider)
                {
                    Some(match confinement {
                        Confinement::Confined => {
                            "This engine's configuration cannot admit the required model and limits."
                        }
                        Confinement::Unconfined => "This engine is not available for this project.",
                    })
                } else if matches!(confinement, Confinement::Confined)
                    && self.model_fixed_by_config(provider)
                {
                    Some("This engine's configuration cannot admit the required model and limits.")
                } else {
                    None
                };
                RepositoryProviderOffer {
                    provider,
                    enabled: refusal.is_none(),
                    model: match confinement {
                        Confinement::Confined => {
                            (provider == Provider::Claude).then_some("claude-sonnet-4-6")
                        }
                        Confinement::Unconfined => None,
                    },
                    refusal,
                }
            })
            .collect();
        Some(RepositoryGroupOffer {
            project: OnboardProject {
                target,
                repositories: RepositorySet::new(
                    observed.repositories.primary_id().clone(),
                    observed
                        .repositories
                        .entries()
                        .iter()
                        .map(|entry| RepositoryEntry {
                            repo_id: entry.repo_id.clone(),
                            value: (),
                        })
                        .collect(),
                )
                .ok()?,
            },
            repositories: observed
                .repositories
                .entries()
                .iter()
                .map(|entry| RepositoryLabel {
                    repo_id: entry.repo_id.clone(),
                    label: entry.value.label.clone(),
                })
                .collect(),
            providers,
        })
    }

    pub(super) fn onboarding_repository_plan(
        &self,
        session: &crate::floor_session::SessionIdentity,
        request: &crate::floor_actions::OnboardRequest,
    ) -> Result<RepositoryHirePlan, crate::floor_actions::OnboardRefusal> {
        use crate::floor_actions::{OnboardRefusal, OnboardTarget};
        let OnboardTarget::Project(project) = &request.target else {
            return Err(OnboardRefusal::Forbidden);
        };
        if !session.is_valid()
            || !self.ctx.switches.load().hire
            || self.ctx.onboarding_index.count(session.digest()) >= 4
        {
            return Err(OnboardRefusal::Forbidden);
        }
        let admission =
            CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories);
        let (profile, observed) = admission
            .current_project_repositories(self.sup.world(), &project.target.project_id)
            .map_err(|_| OnboardRefusal::ProfileChanged)?;
        if observed.lsn != Some(project.target.expected_revision)
            || observed.repositories.primary_id() != project.repositories.primary_id()
            || !observed
                .repositories
                .entries()
                .iter()
                .map(|entry| &entry.repo_id)
                .eq(project
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| &entry.repo_id))
        {
            return Err(OnboardRefusal::ProfileChanged);
        }
        if request.model.is_some() && self.model_fixed_by_config(request.provider) {
            return Err(OnboardRefusal::ModelFixed);
        }
        let offer = self
            .browser_repository_offer(profile)
            .ok_or(OnboardRefusal::ProfileChanged)?;
        let provider = offer
            .providers
            .iter()
            .find(|provider| provider.provider == request.provider)
            .ok_or(OnboardRefusal::Forbidden)?;
        if let Some(refusal) = provider.refusal {
            return Err(OnboardRefusal::RepositoryGroup(refusal));
        }
        if let Some(pinned) = provider.model
            && request.model.as_ref().map(dudes_types::ModelName::as_str) != Some(pinned)
        {
            return Err(OnboardRefusal::RepositoryGroup(
                "This project requires claude-sonnet-4-6 for multi-repository work. Nobody was hired.",
            ));
        }
        let params = HireParams {
            project: Some(project.target.clone()),
            cwd: std::path::PathBuf::new(),
            name: request.name.clone(),
            character: request.character,
            accent: request.accent,
            role: request.role.clone(),
            department: profile.department.clone(),
            provider: request.provider,
            budget: profile.agent_budget,
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: request.model.clone(),
            trusted: true,
            checkout: Some(Checkout::Worktree),
            from: None,
            mcp_servers: Vec::new(),
        };
        self.repository_hire_plan(params)
            .map_err(|_| OnboardRefusal::RepositoryGroup(crate::floor_actions::GROUP_COPY_FAILED))
    }

    pub(super) fn onboard_project_prepared(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &crate::floor_actions::OnboardRequest,
        prepared: &PreparedRepositoryGroup,
    ) -> crate::floor_actions::OnboardStep {
        use crate::floor_actions::{OnboardRefusal, OnboardStep};
        if self
            .ctx
            .onboarding_index
            .attempt(request.attempt_id)
            .is_some()
        {
            return OnboardStep::Answer(Err(OnboardRefusal::Conflict));
        }
        let result = (|| {
            if self.exited || self.ctx.journal_writes_stopped() {
                return Err(OnboardRefusal::UnavailableNoEffect);
            }
            let current = self.onboarding_repository_plan(session, request)?;
            if current != prepared.plan {
                return Err(OnboardRefusal::ProfileChanged);
            }
            let mut hire = self.prepared_repository_hire(prepared).map_err(|_| {
                OnboardRefusal::RepositoryGroup(crate::floor_actions::GROUP_COPY_FAILED)
            })?;
            hire.browser_onboarding = Some(Box::new(dudes_proto::record::BrowserOnboardingHire {
                attempt_id: request.attempt_id,
                request_digest: request.request_digest,
                profile_id: current.project.id,
                session_digest: session.digest(),
                intended_goal: None,
                trust_lsn: None,
            }));
            self.operator_authority.seal();
            self.pump_onboarding(session, Command::HireWithRepositories(Box::new(hire)))?;
            self.ctx
                .onboarding_pause
                .hit(super::OnboardingPausePoint::BetweenEffects);
            self.ctx
                .onboarding_index
                .attempt(request.attempt_id)
                .ok_or(OnboardRefusal::Unavailable)?
                .outcome(request)
        })();
        OnboardStep::Answer(result)
    }

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
        let admission =
            CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories);
        require_group_options(&admission, &params).map_err(|error| refused(error.to_string()))?;
        match self.ctx.current_repositories.hire_policy().confinement() {
            Confinement::Confined => {
                if !params.mcp_servers.is_empty() {
                    return Err(refused(
                        "restricted repository hires cannot enable MCP servers",
                    ));
                }
            }
            Confinement::Unconfined => {}
        }
        self.hire_option_guards(&params)?;
        let (project, repositories) =
            CurrentHireAdmission::new(&self.ctx.enrollment, &self.ctx.current_repositories)
                .current_repository_hire_facts(self.sup.world(), target)
                .map_err(|error| refused(error.to_string()))?;
        match self.ctx.current_repositories.hire_policy().confinement() {
            Confinement::Confined => {}
            Confinement::Unconfined => require_section_fits(
                &self.sup.config().data_dir,
                repositories.entries().iter().map(|entry| &entry.repo_id),
            )
            .map_err(|error| refused(error.to_string()))?,
        }
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
            isolation: params.isolation,
            mcp_servers: {
                let mut names = params.mcp_servers.clone();
                names.sort();
                names.dedup();
                names
            },
            browser_onboarding: None,
            repositories: ScopedRepositories {
                project: plan.project.clone(),
                catalog_lsn: plan.catalog_lsn,
                policy: self.ctx.current_repositories.hire_policy(),
                repositories: prepared.repositories.clone(),
            },
        };
        self.sup
            .world()
            .validate_repository_record(&dudes_proto::Entry {
                at: self.ctx.clock.now(),
                record: Box::new(hire.clone()).into_record(),
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

#[cfg(test)]
mod confinement_tests {
    use super::*;
    use crate::runtime::repository_admission::CurrentRepositories;
    use dudes_proto::record::samples;
    use dudes_proto::{Record, RecordKind};

    #[test]
    fn production_project_hires_are_unconfined() {
        assert_eq!(
            CurrentRepositories::default().hire_policy().confinement(),
            Confinement::Unconfined
        );
    }

    #[test]
    fn only_the_unconfined_gate_is_open_without_the_test_switch() {
        let enrollment = crate::floor_actions::Enrollment::default();
        let mut current = CurrentRepositories::default();
        let admission = CurrentHireAdmission::new(&enrollment, &current);
        admission
            .require_repository_capability(Confinement::Unconfined)
            .unwrap();
        let error = admission
            .require_repository_capability(Confinement::Confined)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "restricted repository execution has no verified provider capability yet"
        );
        current.test_hire_capability = true;
        let admission = CurrentHireAdmission::new(&enrollment, &current);
        for confinement in [Confinement::Confined, Confinement::Unconfined] {
            admission
                .require_repository_capability(confinement)
                .unwrap();
        }
    }

    #[test]
    fn an_unconfined_hire_authority_is_checked_against_the_current_catalog() {
        let Record::AgentHiredWithUnconfinedRepositories(hire) =
            samples::record(RecordKind::AgentHiredWithUnconfinedRepositories)
        else {
            unreachable!()
        };
        let enrollment = crate::floor_actions::Enrollment::default();
        let current = CurrentRepositories::default();
        let error = CurrentHireAdmission::new(&enrollment, &current)
            .require_scoped_hire_authority(&World::default(), &hire.repositories)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("select a project with an explicit repository list"),
            "{error}"
        );
    }

    #[test]
    fn a_hire_whose_copies_could_not_all_be_named_in_the_task_is_refused_before_any_lease() {
        let ids: Vec<_> = (0..8)
            .map(|n| RepoId::parse(&format!("repository-{n}")).unwrap())
            .collect();
        let short = std::path::Path::new("/var/lib/dudes");
        assert!(require_section_fits(short, ids.iter()).is_ok());
        let long = std::path::PathBuf::from(format!("/{}", "d".repeat(260)));
        let error = require_section_fits(&long, ids.iter()).unwrap_err();
        assert_eq!(error, dudes_proto::task::RepositorySectionError::TooLarge);
        assert!(require_section_fits(&long, ids[..2].iter()).is_ok());
    }
}
