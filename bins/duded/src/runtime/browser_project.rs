//! A Floor session's look at a folder and its add or remove of a project, on the loop thread. Each
//! runs the edit `dudes project add` or `dudes project remove` runs, then the reload the CLI asks
//! for, with the reload lock held for the whole of it. A reload the daemon declines puts the file
//! back.

use std::path::Path;
use std::sync::TryLockError;

use dudes_config::check::{changed_check, check_label, suggest_check};
use dudes_config::workspace::{NewWorkspace, WorkspaceError};
use dudes_proto::rpc::floor::EnrollmentApply;
use dudes_proto::{AgentBudget, GoalBudget, Predicate};
use sha2::Digest as _;

use super::{Clock, Daemon, PredicateRunner, ProcessHost, Rng, predicate};
use crate::floor_actions::Profile;
use crate::floor_project::{
    Added, Check, Preview, ProjectAnswer, ProjectRef, ProjectRequest, Reason, Refusal, Removed,
};

/// Most projects the daemon runs.
const MOST_PROJECTS: usize = 32;

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// One admitted project request.
    pub(super) fn browser_project(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &ProjectRequest,
    ) -> Result<ProjectAnswer, Refusal> {
        if !session.is_valid() {
            return Err(Refusal::Forbidden);
        }
        match request {
            ProjectRequest::Preview(request) => self
                .project_preview(&request.path)
                .map(ProjectAnswer::Preview),
            ProjectRequest::Add(request) => {
                self.project_add(session, request).map(ProjectAnswer::Added)
            }
            ProjectRequest::Remove(request) => self
                .project_remove(session, &request.project_id)
                .map(ProjectAnswer::Removed),
        }
        .map_err(Refusal::Reason)
    }

    fn project_preview(&self, typed: &str) -> Result<Preview, Reason> {
        let folder = self.ctx.project_folders.resolve(typed)?;
        let text = folder.to_str().ok_or(Reason::NotAllowed)?.to_owned();
        let running = self
            .ctx
            .enrollment
            .profiles
            .iter()
            .find(|p| p.cwd == folder);
        let (argv, agent, goal) = running.map_or_else(
            || {
                let argv = suggest_check(&folder).unwrap_or_else(changed_check);
                (argv, uncapped_agent(), uncapped_goal())
            },
            |profile| {
                (
                    profile_argv(profile),
                    profile.agent_budget,
                    profile.goal_budget,
                )
            },
        );
        let reason = self.project_readiness(&folder, &argv, &agent, &goal);
        Ok(Preview {
            folder: text,
            already: running.map(project_ref),
            check_label: running.map_or_else(
                || check_label(&argv),
                |profile| profile.checks[0].label.clone(),
            ),
            check: Check::of(argv),
            hireable: reason.is_none(),
            reason,
        })
    }

    fn project_add(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: &crate::floor_project::AddRequest,
    ) -> Result<Added, Reason> {
        let folder = self.ctx.project_folders.resolve(&request.folder)?;
        if let Some(profile) = self
            .ctx
            .enrollment
            .profiles
            .iter()
            .find(|p| p.cwd == folder)
        {
            return Ok(self.added(profile));
        }
        if folder.to_str() != Some(request.folder.as_str()) {
            return Err(Reason::Changed);
        }
        let argv = request.check.argv();
        if argv != suggest_check(&folder).unwrap_or_else(changed_check) {
            return Err(Reason::Changed);
        }
        if self.ctx.enrollment.profiles.len() >= MOST_PROJECTS {
            return Err(Reason::Full);
        }
        let path = self
            .ctx
            .operator_config
            .clone()
            .ok_or(Reason::ConfigChanged)?;
        let lock = self.handle.reload_lock();
        let mut sequence = match lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(Reason::Busy),
        };
        *sequence += 1;

        let previous =
            dudes_config::workspace::read_bytes(&path).map_err(|_| Reason::ConfigChanged)?;
        let mut ids = dudes_config::workspace::enrolled_in(&path, previous.as_deref())
            .map_err(|_| Reason::ConfigChanged)?;
        ids.extend(self.ctx.enrollment.profiles.iter().map(|p| p.id.clone()));
        let id = dudes_config::workspace::project_id(&folder, &ids);
        let label = request.label.clone().unwrap_or_else(|| {
            folder
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| name.len() <= 128 && !name.chars().any(char::is_control))
                .unwrap_or(&id)
                .to_owned()
        });
        let new = NewWorkspace::with_defaults(folder, id.clone(), label, argv);
        let written = dudes_config::workspace::appended(&path, previous.as_deref(), &new).map_err(
            |error| match error {
                WorkspaceError::NotARepository(_) => Reason::NotGit,
                _ => Reason::ConfigChanged,
            },
        )?;
        self.write_and_adopt(
            &path,
            previous.as_deref(),
            &written,
            *sequence,
            std::slice::from_ref(&id),
            &[],
        )?;
        self.resync_floor();
        tracing::info!(
            session = %&crate::enrollment_reload::hex(&session.digest().bytes())[..16],
            project = %id,
            "floor project add"
        );
        let profile = self
            .ctx
            .enrollment
            .profile(&id)
            .ok_or(Reason::ConfigChanged)?;
        Ok(self.added(profile))
    }

    fn project_remove(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        id: &str,
    ) -> Result<Removed, Reason> {
        let profile = self.ctx.enrollment.profile(id).ok_or(Reason::Unknown)?;
        let (project_id, label, cwd) = (
            profile.id.clone(),
            profile.label.clone(),
            profile.cwd.clone(),
        );
        if let Some(reason) = self.removal_blocker(&project_id, &cwd) {
            return Err(reason);
        }
        let path = self
            .ctx
            .operator_config
            .clone()
            .ok_or(Reason::ConfigChanged)?;
        let lock = self.handle.reload_lock();
        let mut sequence = match lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(Reason::Busy),
        };
        *sequence += 1;

        let previous =
            dudes_config::workspace::read_bytes(&path).map_err(|_| Reason::ConfigChanged)?;
        let written = dudes_config::workspace::removed(&path, previous.as_deref(), &project_id)
            .map_err(|_| Reason::ConfigChanged)?;
        self.write_and_adopt(
            &path,
            previous.as_deref(),
            &written,
            *sequence,
            &[],
            std::slice::from_ref(&project_id),
        )?;
        self.resync_floor();
        tracing::info!(
            session = %&crate::enrollment_reload::hex(&session.digest().bytes())[..16],
            project = %project_id,
            "floor project remove"
        );
        Ok(Removed { project_id, label })
    }

    /// Why a project cannot be removed yet: an agent that is not retired works in it, or a ticket
    /// that is not done names it. An agent counts when the projection recorded it under the
    /// project, or when its folder, resolved, is the project's folder or below it. A folder or
    /// projection that cannot be read counts as working, so the project stays.
    fn removal_blocker(&self, id: &str, project: &Path) -> Option<Reason> {
        let world = self.sup.world();
        let works =
            world
                .agents
                .iter()
                .filter(|(_, agent)| !agent.retired)
                .any(|(agent_id, agent)| {
                    let recorded = self.ctx.projections.agent(*agent_id).map_or(true, |row| {
                        row.is_some_and(|row| {
                            row.workspace.is_some_and(|workspace| workspace.id == id)
                        })
                    });
                    let directory = match agent.worktree {
                        None => Some(&agent.cwd),
                        Some(lease) => world.worktrees.get(&lease).map(|lease| &lease.origin),
                    };
                    recorded
                        || directory
                            .and_then(|directory| directory.canonicalize().ok())
                            .is_none_or(|directory| directory.starts_with(project))
                });
        if works {
            return Some(Reason::Working);
        }
        let open = self.ctx.projections.tickets().map_or(true, |tickets| {
            tickets.iter().any(|(card, ticket)| {
                ticket.project == id && card.status != dudes_types::CardStatus::Done
            })
        });
        open.then_some(Reason::OpenTickets)
    }

    /// Write `written`, then have the daemon adopt it. A reload the daemon declines puts the file
    /// back as `previous`, unless something else has written it since.
    fn write_and_adopt(
        &mut self,
        path: &Path,
        previous: Option<&[u8]>,
        written: &[u8],
        seq: u64,
        added: &[String],
        removed: &[String],
    ) -> Result<(), Reason> {
        dudes_config::workspace::replace(path, written).map_err(|_| Reason::ConfigChanged)?;
        if self.adopt(path, written, seq, added, removed) {
            return Ok(());
        }
        let holds_ours = dudes_config::workspace::read_bytes(path)
            .is_ok_and(|now| now.as_deref() == Some(written));
        if holds_ours {
            let _ = dudes_config::workspace::restore(path, previous);
        }
        Err(Reason::ConfigChanged)
    }

    /// The reload the CLI asks for after an edit: adopt exactly `written`, adding and removing the
    /// ids named.
    fn adopt(
        &mut self,
        path: &Path,
        written: &[u8],
        seq: u64,
        added: &[String],
        removed: &[String],
    ) -> bool {
        let sha256 = crate::enrollment_reload::hex(&sha2::Sha256::digest(written));
        let Ok((startup, memory_config)) =
            crate::config::parse_operator_and_memory_with_options(path, Some(written))
        else {
            return false;
        };
        let read = crate::enrollment_reload::ReloadRead {
            seq,
            apply: Some(EnrollmentApply {
                sha256: sha256.clone(),
                added: added.to_vec(),
                removed: removed.to_vec(),
                changed: Vec::new(),
            }),
            sha256,
            path: path.to_path_buf(),
            config: startup.operator,
            memory_config,
            pid: None,
        };
        self.enrollment_reload(&read)
            .is_ok_and(|result| result.applied)
    }

    fn added(&self, profile: &Profile) -> Added {
        let reason = self.project_readiness(
            &profile.cwd,
            &profile_argv(profile),
            &profile.agent_budget,
            &profile.goal_budget,
        );
        Added {
            project_id: profile.id.clone(),
            label: profile.label.clone(),
            check_label: profile.checks[0].label.clone(),
            hireable: reason.is_none(),
            reason,
        }
    }

    /// Why a hire cannot use a project with this check and these ceilings yet: its check cannot
    /// run, or no engine may run without a ceiling.
    fn project_readiness(
        &self,
        cwd: &Path,
        argv: &[String],
        agent: &AgentBudget,
        goal: &GoalBudget,
    ) -> Option<&'static str> {
        if predicate::spec_for(
            argv,
            cwd,
            &self.ctx.predicate_env,
            self.ctx.predicate_path.as_deref(),
        )
        .is_err()
        {
            Some("check_not_runnable")
        } else if self.engines_for(agent, goal).is_empty() {
            Some("ceiling_required")
        } else {
            None
        }
    }
}

fn project_ref(profile: &Profile) -> ProjectRef {
    ProjectRef {
        project_id: profile.id.clone(),
        label: profile.label.clone(),
    }
}

/// The command of a project's own check.
fn profile_argv(profile: &Profile) -> Vec<String> {
    match &profile.checks[0].predicate {
        Predicate::Command { argv, .. } => argv.clone(),
        _ => Vec::new(),
    }
}

const fn uncapped_agent() -> AgentBudget {
    AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    }
}

const fn uncapped_goal() -> GoalBudget {
    GoalBudget {
        usd: None,
        tokens: None,
    }
}
