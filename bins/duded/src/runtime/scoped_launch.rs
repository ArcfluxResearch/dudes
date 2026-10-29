use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

use dudes_proto::instruction::InstructionObservationOwner;
use dudes_proto::repository::{HireRepositories, RepositoryEntry};
use dudes_providers::adapter::AgentCtx;
use dudes_providers::scoped::{ScopedLaunchInputs, ScopedPlatform, ScopedPrivatePaths};
use dudes_supervisor::{Clock, Input, ManagedSpawn, ProcessHost, Rng, Supervisor};
use dudes_types::{Lsn, Mode, Provider, TurnId};

use super::{Ctx, Daemon, PredicateRunner, Shared};
use crate::chat_launch::{ScopedCompletion, ScopedPreparation};
use crate::codex_profile::scoped::{Attempt, PreparedScopedLaunch, ScopedProfile};

pub(crate) enum ScopedReservation {
    Preparing {
        attempt: Attempt,
        browser_generation: Option<u64>,
    },
    Ready {
        browser_generation: u64,
        prepared: Box<PreparedScopedLaunch>,
        outbox: Arc<crate::mailbox::OutboxGrant>,
    },
    Dispatching {
        attempt: Attempt,
        browser_generation: Option<u64>,
    },
}

#[cfg(test)]
impl From<Attempt> for ScopedReservation {
    fn from(attempt: Attempt) -> Self {
        Self::Preparing {
            attempt,
            browser_generation: None,
        }
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn complete_scoped_launch(
        &mut self,
        completion: ScopedCompletion,
    ) -> anyhow::Result<()> {
        let attempt = completion.attempt;
        self.ctx.browser_launches.finish_scoped(attempt);
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
            self.pump_runtime_input(input)?;
        }
        if matches!(self.ctx.scoped_launches.get(&attempt.turn), Some(ScopedReservation::Ready { prepared, .. }) if prepared.attempt == attempt)
        {
            if current_attempt(self.sup.world(), attempt)
                && !self.sup.world().browser_issue_cancelled(attempt.turn)
            {
                self.pump(Input::LaunchPreparationFailed {
                    turn: attempt.turn,
                    started_lsn: attempt.started,
                    reason: "scoped browser preparation no longer admits dispatch".into(),
                })?;
            }
            if !current_attempt(self.sup.world(), attempt)
                || self.sup.world().browser_issue_cancelled(attempt.turn)
            {
                self.ctx.scoped_launches.remove(&attempt.turn);
            }
        }
        Ok(())
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn prepare_scoped_turn<R: Rng>(
        &mut self,
        turn: TurnId,
        browser_generation: Option<u64>,
        launch: &ManagedSpawn,
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
        if let Some(row) = sup.world().turns.get(&turn)
            && let Err(refusal) = self.admit_prior_attempt(sup, row.agent, Some(turn))
        {
            queue.push_back(Input::LaunchAdmissionRefused {
                agent: row.agent,
                turn: Some(turn),
                refusal,
                dispatch_token: None,
            });
            return;
        }
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
        let prepared = self.scoped_request(attempt, launch, mcp_grant, sup);
        match prepared {
            Ok(request) => {
                self.scoped_launches.insert(
                    turn,
                    ScopedReservation::Preparing {
                        attempt,
                        browser_generation,
                    },
                );
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
        launch: &ManagedSpawn,
        mcp_grant: Option<Lsn>,
        sup: &Supervisor<Shared<C>, R>,
    ) -> io::Result<ScopedPreparation> {
        let refused = || io::Error::other("scoped turn authority is unavailable");
        let row = sup.world().turns.get(&attempt.turn).ok_or_else(refused)?;
        let agent = sup.world().agents.get(&row.agent).ok_or_else(refused)?;
        let ManagedSpawn::Scoped {
            task,
            session,
            selection,
            env,
        } = launch
        else {
            return Err(refused());
        };
        if mcp_grant.is_some()
            || task != &row.task
            || row.model_selection.as_ref() != Some(selection)
            || !sup.scoped_launch_session_matches(attempt.turn, session)
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
        let (profile, profile_home) = match agent.provider {
            Provider::Codex => {
                let profile = self.children.codex_profile().ok_or_else(refused)?;
                let home = profile.scoped_home().to_owned();
                (ScopedProfile::Codex(profile), home)
            }
            Provider::Claude => (
                ScopedProfile::Claude(self.children.private_roots()),
                agent_dir.join("profile"),
            ),
            _ => return Err(refused()),
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
            daemon_env: env,
            sandbox: agent.sandbox,
            trusted: agent.trusted,
            max_turns: agent.max_turns,
            max_budget: agent.max_budget,
            mcp_servers: &[],
        };
        let adapter = sup.adapters().get(agent.provider).ok_or_else(refused)?;
        let launch = adapter
            .managed_scoped(
                &ctx,
                &ScopedLaunchInputs {
                    grant,
                    hired: &hired,
                    origin_git_dirs: &git,
                    private: ScopedPrivatePaths {
                        home: agent_dir.join("home"),
                        profile: profile_home,
                        work: agent_dir.join("work"),
                        outbox: agent_dir.join("outbox"),
                        temp: agent_dir.join("tmp"),
                    },
                    mode: Mode::Managed,
                    platform: native_platform(),
                },
                task,
                session,
                selection,
            )
            .map_err(|_| refused())?;
        Ok(ScopedPreparation {
            attempt,
            launch,
            profile,
            outbox: self
                .mailbox
                .as_ref()
                .ok_or_else(refused)?
                .admit_outbox(row.agent)?,
        })
    }

    fn finish_scoped_launch<R: Rng>(
        &mut self,
        completion: ScopedCompletion,
        queue: &mut VecDeque<Input>,
        sup: &mut Supervisor<Shared<C>, R>,
    ) -> anyhow::Result<()> {
        let attempt = completion.attempt;
        let Some(ScopedReservation::Preparing {
            attempt: reserved,
            browser_generation,
        }) = self.scoped_launches.get(&attempt.turn)
        else {
            return Ok(());
        };
        if *reserved != attempt {
            return Ok(());
        }
        let browser_generation = *browser_generation;
        if !current_attempt(sup.world(), attempt)
            || self.journal_writes_stopped()
            || self.closing_failed_after.is_some()
        {
            self.scoped_launches.remove(&attempt.turn);
            return Ok(());
        }
        let agent = sup.world().turns[&attempt.turn].agent;
        if let Err(refusal) = self.admit_prior_attempt(sup, agent, Some(attempt.turn)) {
            self.scoped_launches.remove(&attempt.turn);
            queue.push_back(Input::LaunchAdmissionRefused {
                agent,
                turn: Some(attempt.turn),
                refusal,
                dispatch_token: None,
            });
            return Ok(());
        }
        if !self.recovery_dispatch_allowed(sup, attempt.turn) {
            self.scoped_launches.remove(&attempt.turn);
            preparation_failed(
                queue,
                attempt,
                "scoped preparation no longer has execution permission",
            );
            return Ok(());
        }
        let prepared = match completion.prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                self.scoped_launches.remove(&attempt.turn);
                preparation_failed(queue, attempt, &reason);
                return Ok(());
            }
        };
        if prepared.attempt != attempt || !self.scoped_prepared_current(sup, &prepared) {
            self.scoped_launches.remove(&attempt.turn);
            preparation_failed(
                queue,
                attempt,
                "scoped launch authority changed during preparation",
            );
            return Ok(());
        }
        if let Some(generation) = browser_generation {
            self.scoped_launches.insert(
                attempt.turn,
                ScopedReservation::Ready {
                    browser_generation: generation,
                    prepared: Box::new(prepared),
                    outbox: completion.outbox,
                },
            );
            queue.push_back(Input::BrowserScopedLaunchReady {
                turn: attempt.turn,
                generation,
            });
            return Ok(());
        }
        self.dispatch_scoped_prepared(&prepared, completion.outbox, None, queue, sup)
    }

    pub(super) fn dispatch_scoped_turn<R: Rng>(
        &mut self,
        turn: TurnId,
        queue: &mut VecDeque<Input>,
        sup: &mut Supervisor<Shared<C>, R>,
    ) -> anyhow::Result<()> {
        let Some(reservation) = self.scoped_launches.remove(&turn) else {
            return Ok(());
        };
        let ScopedReservation::Ready {
            browser_generation,
            prepared,
            outbox,
        } = reservation
        else {
            self.scoped_launches.insert(turn, reservation);
            return Ok(());
        };
        self.dispatch_scoped_prepared(&prepared, outbox, Some(browser_generation), queue, sup)
    }

    fn scoped_prepared_current<R: Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        prepared: &PreparedScopedLaunch,
    ) -> bool {
        sup.world().turns.get(&prepared.attempt.turn).is_some_and(|row| {
            current_attempt(sup.world(), prepared.attempt)
                && self.repository_turn_allowed(sup.world(), prepared.attempt.turn)
                && row.repositories.as_ref().is_some_and(|authority| {
                    authority.grant() == Some(&prepared.launch.inventory.grant)
                        && matches!(authority.hire(), HireRepositories::Scoped(hire) if hire.recorded() == prepared.launch.inventory.hired)
                })
        })
    }

    fn dispatch_scoped_prepared<R: Rng>(
        &mut self,
        prepared: &PreparedScopedLaunch,
        outbox: Arc<crate::mailbox::OutboxGrant>,
        browser_generation: Option<u64>,
        queue: &mut VecDeque<Input>,
        sup: &mut Supervisor<Shared<C>, R>,
    ) -> anyhow::Result<()> {
        let attempt = prepared.attempt;
        if !current_attempt(sup.world(), attempt) {
            return Ok(());
        }
        let agent = sup.world().turns[&attempt.turn].agent;
        if let Err(refusal) = self.admit_prior_attempt(sup, agent, Some(attempt.turn)) {
            self.scoped_launches.remove(&attempt.turn);
            queue.push_back(Input::LaunchAdmissionRefused {
                agent,
                turn: Some(attempt.turn),
                refusal,
                dispatch_token: None,
            });
            return Ok(());
        }
        if !self.recovery_dispatch_allowed(sup, attempt.turn)
            || !self.scoped_prepared_current(sup, prepared)
        {
            self.scoped_launches.remove(&attempt.turn);
            preparation_failed(
                queue,
                attempt,
                "scoped launch authority changed before dispatch",
            );
            return Ok(());
        }
        self.scoped_launches.insert(
            attempt.turn,
            ScopedReservation::Dispatching {
                attempt,
                browser_generation,
            },
        );
        let spec = prepared.launch.spec.clone();
        let checked = self
            .children
            .prepare_scoped_launch(prepared, outbox)
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

    pub(super) fn discard_scoped_dispatch(&mut self, turn: TurnId) {
        if matches!(
            self.scoped_launches.get(&turn),
            Some(ScopedReservation::Dispatching { .. })
        ) {
            self.scoped_launches.remove(&turn);
        }
    }

    pub(super) fn claim_scoped_dispatch<R: Rng>(
        &mut self,
        turn: TurnId,
        scoped: bool,
        sup: &mut Supervisor<Shared<C>, R>,
    ) -> bool {
        let Some(reservation) = self.scoped_launches.remove(&turn) else {
            return !scoped;
        };
        let ScopedReservation::Dispatching {
            attempt,
            browser_generation,
        } = reservation
        else {
            self.scoped_launches.insert(turn, reservation);
            return false;
        };
        current_attempt(sup.world(), attempt)
            && self.recovery_dispatch_allowed(sup, turn)
            && self.with_recovery_facts(sup, &[], |sup, operation| {
                sup.claim_browser_dispatch(turn, browser_generation, operation)
            })
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
