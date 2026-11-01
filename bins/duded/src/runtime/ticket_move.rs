//! An agent changing project to start a ticket. A move is its own step of the queue pass: when
//! the next ticket's agent is in another project and everything passes, one `AgentMoved` is
//! appended and the next pass starts the ticket as it starts any other. A crash between the two
//! records leaves an idle agent in the new project, which the next pass finishes.

use dudes_proto::placement::AgentMove;
use dudes_proto::record::RecordedWorkspace;
use dudes_proto::{AgentBudget, CardView, Predicate, TicketReason, TicketRow};
use dudes_providers::adapter::ContextMode;
use dudes_types::{AgentId, IsolationMode, Mode, Provider};

use dudes_supervisor::world::AgentRow;

use super::{Clock, Daemon, PredicateRunner, ProcessHost, Rng, RpcError};
use crate::floor_actions::Profile;

/// The sentence that opens the first task an agent gets in a project it has no conversation in.
pub(super) const FIRST_TASK_NOTE: &str = "You have just moved here from another project. Your earlier projects are not in this conversation, and your memory carries across.";

/// Why an agent cannot take a ticket in another project.
pub(super) struct MoveStop {
    /// What the ticket shows when the move is refused for good.
    pub(super) reason: TicketReason,
    /// The cause, in words for the operator.
    pub(super) why: String,
    /// The cause may pass by itself, so the ticket waits instead of leaving the queue.
    pub(super) transient: bool,
}

fn refuse(reason: TicketReason, why: impl Into<String>) -> MoveStop {
    MoveStop {
        reason,
        why: why.into(),
        transient: false,
    }
}

/// The tighter of two ceilings on each unit, where an unset unit is no cap.
fn tighter<T: Ord>(own: Option<T>, project: Option<T>) -> Option<T> {
    match (own, project) {
        (Some(own), Some(project)) => Some(own.min(project)),
        (own, project) => own.or(project),
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// The project `agent` works in now, when it has one.
    pub(super) fn agent_project(&self, agent: AgentId) -> Option<String> {
        self.sup
            .world()
            .agents
            .get(&agent)?
            .repositories
            .workspace()
            .map(|workspace| workspace.id.clone())
    }

    /// The move that would take `agent` into `project`, or why it cannot go. With a `start`
    /// check, the admission is the one made just before the move: the folder is judged as the
    /// ticket will run in it, and the move is validated against the history it would join.
    pub(super) fn plan_move(
        &self,
        agent: AgentId,
        project: &str,
        start: Option<&Predicate>,
    ) -> Result<AgentMove, MoveStop> {
        let world = self.sup.world();
        let row = world
            .agents
            .get(&agent)
            .filter(|row| !row.retired)
            .ok_or_else(|| {
                refuse(
                    TicketReason::ProjectUnavailable,
                    "That agent is not available.",
                )
            })?;
        let name = &row.name;
        let unfit = |why: &str| {
            refuse(
                TicketReason::ProjectUnavailable,
                format!("{name} cannot move to {project}: {why}."),
            )
        };
        let from = row
            .repositories
            .singleton()
            .ok_or_else(|| unfit("it works in several repositories"))?
            .workspace()
            .ok_or_else(|| unfit("it was hired before projects were recorded"))?;
        if from.id == project {
            return Err(unfit("it is already there"));
        }
        let profile = self
            .ctx
            .enrollment
            .profile(project)
            .ok_or_else(|| unfit("the project is not set up any more"))?;
        self.project_admits(row, profile)
            .map_err(|why| unfit(&why))?;
        let (cwd, worktree) = self
            .landing(agent, row, profile)
            .map_err(|why| unfit(&why))?;
        let to = AgentMove {
            cwd,
            workspace: RecordedWorkspace {
                id: profile.id.clone(),
                label: profile.label.clone(),
            },
            worktree,
            budget: AgentBudget {
                usd_per_day: tighter(
                    row.hired_budget.usd_per_day,
                    profile.agent_budget.usd_per_day,
                ),
                tokens_per_day: tighter(
                    row.hired_budget.tokens_per_day,
                    profile.agent_budget.tokens_per_day,
                ),
            },
        };
        let Some(check) = start else {
            return Ok(to);
        };
        if self.refuse_overlap(&to.cwd, to.worktree).is_err() {
            return Err(unfit("the folder overlaps another agent's copy"));
        }
        if let Err(refusal) = self.ctx.admit_shared_folder(
            &self.sup,
            Some(agent),
            row.provider,
            row.mcp.isolation,
            Mode::Managed,
            &to.cwd,
        ) {
            return Err(MoveStop {
                transient: true,
                ..unfit(&refusal.to_string())
            });
        }
        if let Err(cause) = self.check_can_run_in(&to.cwd, check) {
            return Err(refuse(
                TicketReason::CheckCannotRun,
                format!("{name} cannot move to {project}: {}", cause.message),
            ));
        }
        let entry = dudes_proto::Entry {
            at: self.ctx.clock.now(),
            record: dudes_proto::Record::AgentMoved {
                agent,
                to: to.clone(),
            },
        };
        world
            .validate_move_record(&entry)
            .map_err(|failure| unfit(&failure.to_string()))?;
        Ok(to)
    }

    /// Where the agent lands in the project: the own copy it left there if it holds one, else the
    /// project's shared folder. A move never makes a copy.
    fn landing(
        &self,
        agent: AgentId,
        row: &AgentRow,
        profile: &Profile,
    ) -> Result<(std::path::PathBuf, Option<dudes_types::WorktreeId>), String> {
        let Some(stored) = row
            .other_projects
            .get(&profile.id)
            .filter(|stored| stored.repository.worktree().is_some())
        else {
            return Ok((profile.cwd.clone(), None));
        };
        let copy = stored.repository.worktree();
        let held = copy
            .and_then(|id| self.sup.world().worktrees.get(&id))
            .is_some_and(|lease| lease.agent == Some(agent) && lease.removed.is_none());
        if held {
            Ok((stored.repository.cwd().to_path_buf(), copy))
        } else {
            Err("its own copy there is gone".to_owned())
        }
    }

    /// What a hire into the shared folder of `profile` admits, asked of an agent that already
    /// exists: the project is a single repository that can take a new agent, offers the agent's
    /// engine, and the agent's isolation and MCP servers are the ones such a hire runs with.
    fn project_admits(&self, row: &AgentRow, profile: &Profile) -> Result<(), String> {
        if profile.repositories.is_some() {
            return Err(
                "the project holds several repositories, which a moved agent does not take"
                    .to_owned(),
            );
        }
        if !self.onboarding_profile_eligible(profile)
            || self
                .ctx
                .require_current_hire(self.sup.world(), &profile.cwd)
                .is_err()
        {
            return Err("the project cannot take a new agent now, because its folder or its check is not usable".to_owned());
        }
        if !self.offered_engines(profile).contains(&row.provider) {
            return Err(format!("the project does not offer {}", row.provider));
        }
        let adapter = self
            .sup
            .adapters()
            .get(row.provider)
            .ok_or_else(|| "this daemon has no adapter for its engine".to_owned())?;
        if row.provider == Provider::Claude {
            let configured = match adapter.context_mode(Mode::Managed) {
                ContextMode::ClaudeManaged(mode) => mode,
                ContextMode::Other | ContextMode::ClaudeAttached => IsolationMode::Default,
            };
            if row.mcp.isolation.unwrap_or(configured) != IsolationMode::Default {
                return Err(
                    "its isolation is not the default one, which a shared project folder requires"
                        .to_owned(),
                );
            }
        }
        let rules = dudes_providers::mcp::Rules {
            support: adapter.mcp_support(),
            default_isolation: true,
            trusted: false,
        };
        self.sup
            .config()
            .mcp
            .admit(&row.mcp.desired, rules)
            .map(|_| ())
            .map_err(|refusal| format!("its MCP servers are not admitted there: {refusal}"))
    }

    /// The agent has worked in another project and has no conversation here yet, so the task it
    /// is about to get is its first in this place.
    pub(super) fn arrived_without_a_conversation(&self, agent: AgentId) -> bool {
        self.sup
            .world()
            .agents
            .get(&agent)
            .is_some_and(|row| row.session.is_none() && !row.other_projects.is_empty())
    }

    /// The refusal a give gets when the agent could not move to the ticket's project.
    pub(super) fn give_refusal(stop: &MoveStop) -> RpcError {
        RpcError {
            message: stop.why.clone(),
            ..super::tickets::refusal(
                dudes_proto::TicketRefusal::AgentUnavailable,
                dudes_types::CardStatus::Todo,
            )
        }
    }

    /// Take one step toward starting `view`: move its agent to the ticket's project. Answers
    /// whether the journal moved. A refusal that will not pass sends the ticket to Needs you with
    /// the reason, as a start that cannot happen does.
    pub(super) fn move_for_ticket(
        &mut self,
        view: &CardView,
        ticket: &TicketRow,
        agent: AgentId,
    ) -> Result<bool, RpcError> {
        let project = ticket.repositories.project();
        if self.sup.recovery_invalidations_owed(agent) {
            return Ok(false);
        }
        let to = match self.plan_move(agent, project, Some(&ticket.done_when.predicate)) {
            Ok(to) => to,
            Err(stop) if stop.transient => return Ok(false),
            Err(stop) => {
                tracing::info!(%agent, project, why = %stop.why, "an agent could not move to start its ticket");
                self.refuse_start(view, stop.reason)?;
                return Ok(true);
            }
        };
        let before = self.sup.world().last_applied_lsn;
        self.pump_or_refuse(dudes_supervisor::Input::Command(
            dudes_supervisor::input::Command::MoveAgent {
                agent,
                to: Box::new(to),
            },
        ))?;
        Ok(self.sup.world().last_applied_lsn != before)
    }
}
