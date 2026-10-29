use std::collections::VecDeque;
use std::io;

use dudes_proto::instruction::InstructionObservationOwner;
use dudes_proto::repository::{HireRepositories, RepositoryEntry};
use dudes_providers::adapter::{AgentCtx, TurnSession};
use dudes_providers::launch::{LaunchInput, LaunchSpec};
use dudes_providers::scoped::{ScopedLaunchInputs, ScopedPlatform, ScopedPrivatePaths};
use dudes_supervisor::{Clock, Input, ProcessHost, Rng, Supervisor};
use dudes_types::{Lsn, Mode, Provider, TurnId};

use super::{Ctx, Daemon, PredicateRunner, Shared};
use crate::chat_launch::{ScopedCompletion, ScopedPreparation};
use crate::codex_profile::scoped::{Attempt, ScopedProfile};

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn complete_scoped_launch(
        &mut self,
        completion: ScopedCompletion,
    ) -> anyhow::Result<()> {
        self.ctx.browser_launches.finish_scoped(completion.attempt);
        if self.ctx.journal_writes_stopped() || self.ctx.closing_failed_after.is_some() {
            self.ctx.scoped_launches.remove(&completion.attempt.turn);
            return Ok(());
        }
        if !self.sup.effects().is_empty() {
            let mut owed = VecDeque::new();
            let mut acknowledgments = false;
            let mut requests = std::collections::BTreeSet::new();
            let executed = self.execute_pump_effects(
                &mut owed,
                &mut super::PumpReport::Discard,
                false,
                &mut acknowledgments,
                &mut requests,
            );
            self.fence_failed_pump(
                super::PumpOrigin::Ordinary,
                &executed,
                acknowledgments,
                &owed,
                requests,
            );
            executed?;
            self.pump_inputs(
                &mut super::PumpReport::Discard,
                super::PumpOrigin::Ordinary,
                owed,
                &[],
                |_, _| Ok(super::Handled::default()),
            )?;
        }
        let mut inputs = VecDeque::new();
        self.ctx
            .finish_scoped_launch(completion, &mut inputs, &mut self.sup)?;
        for input in inputs {
            self.pump(input)?;
        }
        Ok(())
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn prepare_scoped_turn<R: Rng>(
        &mut self,
        turn: TurnId,
        spec: &LaunchSpec,
        mcp_grant: Option<Lsn>,
        queue: &mut VecDeque<Input>,
        sup: &Supervisor<Shared<C>, R>,
    ) {
        if !self.recovery_dispatch_allowed(sup, turn) {
            return;
        }
        let Some((started, physical)) = sup.world().instructions.current_for_turn(turn) else {
            queue.push_back(Input::SpawnFailed {
                turn,
                reason: "scoped physical launch attempt is unavailable".into(),
            });
            return;
        };
        let Some(generation) = self.scoped_generation.checked_add(1) else {
            preparation_failed(
                queue,
                Attempt {
                    turn,
                    generation: self.scoped_generation,
                    started,
                    owner: physical.owner,
                },
                "scoped preparation generation is exhausted",
            );
            return;
        };
        self.scoped_generation = generation;
        let attempt = Attempt {
            turn,
            generation,
            started,
            owner: physical.owner,
        };
        if !self.repository_turn_allowed(sup.world(), turn) {
            preparation_failed(queue, attempt, "scoped repository admission is unavailable");
            return;
        }
        if self.scoped_launches.contains_key(&turn) {
            preparation_failed(
                queue,
                attempt,
                "scoped launch already has a preparation reservation",
            );
            return;
        }
        let prepared = self.scoped_request(attempt, spec, mcp_grant, sup);
        match prepared {
            Ok(request) => {
                self.scoped_launches.insert(turn, attempt);
                if !self.browser_launches.prepare_scoped(request) {
                    self.scoped_launches.remove(&turn);
                    preparation_failed(
                        queue,
                        attempt,
                        "scoped launch preparation capacity is unavailable",
                    );
                }
            }
            Err(_) => preparation_failed(queue, attempt, "scoped launch preparation was refused"),
        }
    }

    fn scoped_request<R: Rng>(
        &self,
        attempt: Attempt,
        spec: &LaunchSpec,
        mcp_grant: Option<Lsn>,
        sup: &Supervisor<Shared<C>, R>,
    ) -> io::Result<ScopedPreparation> {
        let refused = || io::Error::other("scoped turn authority is unavailable");
        let row = sup.world().turns.get(&attempt.turn).ok_or_else(refused)?;
        let agent = sup.world().agents.get(&row.agent).ok_or_else(refused)?;
        if mcp_grant.is_some()
            || !matches!(attempt.owner, InstructionObservationOwner::Managed { agent, turn, .. } if agent == row.agent && turn == attempt.turn)
        {
            return Err(refused());
        }
        let authority = row
            .repositories
            .as_ref()
            .filter(|authority| authority.matches_hire(agent.mcp.hire_lsn, &agent.repositories))
            .ok_or_else(refused)?;
        let grant = authority.grant().ok_or_else(refused)?;
        let HireRepositories::Scoped(hire) = authority.hire() else {
            return Err(refused());
        };
        let agent_dir = self.agents_root.join(row.agent.to_string());
        let (profile, profile_home, session) = match agent.provider {
            Provider::Codex => {
                let LaunchInput::Protocol(startup) = &spec.stdin else {
                    return Err(refused());
                };
                let profile = self.children.codex_profile().ok_or_else(refused)?;
                let home = profile.scoped_home().to_owned();
                (
                    ScopedProfile::Codex(profile),
                    home,
                    startup.session().clone(),
                )
            }
            Provider::Claude => (
                ScopedProfile::Claude(self.children.private_roots()),
                agent_dir.join("profile"),
                claude_session(spec)?,
            ),
            _ => return Err(refused()),
        };
        let private = ScopedPrivatePaths {
            home: agent_dir.join("home"),
            profile: profile_home,
            work: agent_dir.join("work"),
            outbox: agent_dir.join("outbox"),
            temp: agent_dir.join("tmp"),
        };
        let hired = hire.recorded();
        let git = hired
            .repositories
            .entries()
            .iter()
            .map(|entry| {
                crate::worktree::plain_git_dir(&entry.value.origin.directory)
                    .map(|value| RepositoryEntry {
                        repo_id: entry.repo_id.clone(),
                        value,
                    })
                    .ok_or_else(refused)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let ctx = AgentCtx {
            agent: row.agent,
            cwd: &grant.repositories.primary().execution.directory,
            agent_dir: &agent_dir,
            daemon_env: &spec.env,
            sandbox: agent.sandbox,
            trusted: agent.trusted,
            max_turns: agent.max_turns,
            max_budget: agent.max_budget,
            mcp_servers: &[],
        };
        let launch = sup
            .adapters()
            .get(agent.provider)
            .ok_or_else(refused)?
            .managed_scoped(
                &ctx,
                &ScopedLaunchInputs {
                    grant,
                    hired: &hired,
                    origin_git_dirs: &git,
                    private,
                    mode: Mode::Managed,
                    platform: native_platform(),
                },
                &row.task,
                &session,
                row.model_selection.as_ref().ok_or_else(refused)?,
            )
            .map_err(|_| refused())?;
        let outbox = self
            .mailbox
            .as_ref()
            .ok_or_else(refused)?
            .admit_outbox(row.agent)?;
        Ok(ScopedPreparation {
            attempt,
            launch,
            profile,
            outbox,
        })
    }

    fn finish_scoped_launch<R: Rng>(
        &mut self,
        completion: ScopedCompletion,
        queue: &mut VecDeque<Input>,
        sup: &mut Supervisor<Shared<C>, R>,
    ) -> anyhow::Result<()> {
        let attempt = completion.attempt;
        if self.scoped_launches.get(&attempt.turn) != Some(&attempt) {
            return Ok(());
        }
        self.scoped_launches.remove(&attempt.turn);
        if !current_attempt(sup.world(), attempt)
            || self.journal_writes_stopped()
            || self.closing_failed_after.is_some()
            || !self.recovery_dispatch_allowed(sup, attempt.turn)
        {
            return Ok(());
        }
        let agent = sup.world().turns[&attempt.turn].agent;
        if let Err(refusal) = self.admit_prior_attempt(sup, agent, Some(attempt.turn)) {
            queue.push_back(Input::LaunchAdmissionRefused {
                agent,
                turn: Some(attempt.turn),
                refusal,
                dispatch_token: None,
            });
            return Ok(());
        }
        let prepared = match completion.prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                preparation_failed(queue, attempt, &reason);
                return Ok(());
            }
        };
        let row = &sup.world().turns[&attempt.turn];
        let admitted = prepared.attempt == attempt
            && self.repository_turn_allowed(sup.world(), attempt.turn)
            && row.repositories.as_ref().is_some_and(|authority| {
                authority.grant() == Some(&prepared.launch.inventory.grant)
                    && matches!(authority.hire(), HireRepositories::Scoped(hire) if hire.recorded() == prepared.launch.inventory.hired)
            })
            && self.admit_prior_attempt(sup, row.agent, Some(attempt.turn)).is_ok();
        if !admitted {
            preparation_failed(
                queue,
                attempt,
                "scoped launch authority changed during preparation",
            );
            return Ok(());
        }
        let spec = prepared.launch.spec.clone();
        let checked = self
            .children
            .prepare_scoped_launch(&prepared, completion.outbox)
            .map(|()| {
                self.children.read_next_for(
                    super::turn_reader(sup, attempt.turn, &spec),
                    &spec,
                    None,
                );
                spec
            });
        self.launch_prepared_host(
            attempt.turn,
            checked,
            &dudes_providers::adapter::ManagedInstall::default(),
            None,
            queue,
            sup,
        )
    }
}

pub(super) fn native_platform() -> ScopedPlatform {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        ScopedPlatform::MacOsArm64
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        ScopedPlatform::LinuxX86_64
    } else {
        ScopedPlatform::Unsupported
    }
}

fn claude_session(spec: &LaunchSpec) -> io::Result<TurnSession> {
    let refused = || io::Error::other("scoped Claude session is unavailable");
    let dudes_providers::adapter::SessionAcquisition::Pinned {
        pin,
        resume: resume_flag,
    } = dudes_providers::claude::SESSION_ACQUISITION
    else {
        return Err(refused());
    };
    let args: Vec<_> = spec.argv.iter().take_while(|part| *part != "--").collect();
    let mut sessions = args.windows(2).filter_map(|parts| {
        let flag = parts[0].to_str()?;
        let resume = if flag == pin {
            false
        } else if flag == resume_flag {
            true
        } else {
            return None;
        };
        Some((resume, parts[1].to_str()))
    });
    let (resume, id) = sessions.next().ok_or_else(refused)?;
    if sessions.next().is_some() {
        return Err(refused());
    }
    let id = dudes_types::SessionId::new(id.filter(|id| !id.is_empty()).ok_or_else(refused)?);
    Ok(if resume {
        TurnSession::Resume(id)
    } else {
        TurnSession::First(id)
    })
}

fn current_attempt(world: &dudes_supervisor::World, attempt: Attempt) -> bool {
    world
        .instructions
        .current_for_turn(attempt.turn)
        .is_some_and(|(started, physical)| {
            started == attempt.started
                && physical.owner == attempt.owner
                && physical.finished.is_none()
                && physical.spawn_lsn.is_none()
        })
}

fn preparation_failed(queue: &mut VecDeque<Input>, attempt: Attempt, reason: &str) {
    queue.push_back(Input::LaunchPreparationFailed {
        turn: attempt.turn,
        started_lsn: attempt.started,
        reason: reason.to_owned(),
    });
}
