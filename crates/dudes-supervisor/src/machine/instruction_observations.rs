use super::{
    Adapter, AgentCtx, AgentId, BTreeMap, BTreeSet, Clock, Effect, Entry, EnvVar, ExitStatus,
    GoalId, LoopDecision, Lsn, Mode, ModelSelection, Path, PathBuf, PreparedLaunch, Provenance,
    Provider, Record, Rng, Supervisor, Then, TurnId, Usd, WorktreeId,
};
use crate::input::InstructionRequest;
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionComparison, InstructionCoverageGap, InstructionEndBoundary,
    InstructionGapKind, InstructionIssueAssociation, InstructionObservationOwner,
    InstructionPathContext, InstructionSampleUsage, InstructionSettlement, InstructionSnapshot,
    InstructionSnapshotSource, InstructionTicketAssociation, InstructionUnknownReason,
    MAX_INSTRUCTION_INPUTS, MAX_INSTRUCTION_PATH_BYTES,
};
use std::sync::Arc;

/// Bounded metadata for pure declaration after the daemon resolves its execution folder.
pub struct InstructionSource {
    /// Recorded owner.
    pub agent: AgentId,
    /// Actual engine.
    pub provider: Provider,
    /// Recorded execution path before daemon resolution.
    pub cwd: PathBuf,
    /// Actual recorded owned lease, when present.
    pub lease: Option<WorktreeId>,
    /// Current managed or inherited Attached composition.
    pub mode: Mode,
    agent_dir: PathBuf,
    adapter: Arc<dyn Adapter>,
    environment: Vec<EnvVar>,
    clipped: bool,
    sandbox: Option<bool>,
    trusted: bool,
    max_turns: Option<u32>,
    max_budget: Option<Usd>,
}

impl InstructionSource {
    /// Declare against the actual resolved folder; unbounded supplied metadata remains unknown.
    #[must_use]
    pub fn declaration(
        &self,
        cwd: &Path,
        selection: &ModelSelection,
    ) -> dudes_providers::instructions::InstructionDeclaration {
        let ctx = AgentCtx {
            agent: self.agent,
            cwd,
            agent_dir: &self.agent_dir,
            daemon_env: &self.environment,
            sandbox: self.sandbox,
            trusted: self.trusted,
            max_turns: self.max_turns,
            max_budget: self.max_budget,
            mcp_servers: &[],
        };
        let mut declaration = self
            .adapter
            .instruction_manifest(&ctx, self.mode, selection);
        if self.clipped {
            declaration
                .gaps
                .push(dudes_providers::instructions::DeclarationGap::DeclarationLimit);
        }
        declaration
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NoHostAttempt {
    pub(super) owner: InstructionObservationOwner,
    pub(super) ticket: Option<InstructionTicketAssociation>,
    pub(super) baseline: InstructionBaselineRef,
}

#[derive(Default)]
pub(super) struct Observations {
    awaiting: BTreeMap<InstructionRequest, Awaiting>,
    pub(super) pending_agents: BTreeSet<AgentId>,
    pub(super) ready_turns: BTreeSet<TurnId>,
    pub(super) ending: BTreeSet<InstructionBaselineRef>,
    next_request: u64,
    recovery: Recovery,
    recovery_checks: BTreeSet<RecoveryCheck>,
    recovery_tickets: BTreeMap<InstructionRequest, TicketResolution>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RecoveryCheck {
    agent: AgentId,
    goal: GoalId,
    turn: TurnId,
    end_lsn: Lsn,
}

#[derive(Default)]
enum Recovery {
    #[default]
    NotStarted,
    Scanning,
    Awaiting,
    Checking(GoalId),
    Complete,
}

impl Observations {
    pub(super) fn recovering(&self) -> bool {
        matches!(
            self.recovery,
            Recovery::Scanning | Recovery::Awaiting | Recovery::Checking(_)
        )
    }

    pub(super) fn dispatch_pending(
        &self,
        world: &crate::World,
        spawning: &BTreeSet<TurnId>,
    ) -> bool {
        spawning
            .iter()
            .any(|turn| world.instructions.current_for_turn(*turn).is_some())
            || self.awaiting.values().any(|awaiting| {
                matches!(awaiting, Awaiting::Start(Starting {
                owner: InstructionObservationOwner::Managed { turn, .. },
                action: StartAction::Retry { .. },
                ..
            }) if spawning.contains(turn))
            })
    }

    pub(super) fn recovering_check(&self, goal: GoalId) -> bool {
        matches!(self.recovery, Recovery::Checking(checking) if checking == goal)
    }
}

#[derive(Debug)]
pub(super) struct Starting {
    owner: InstructionObservationOwner,
    goal: Option<GoalId>,
    ticket: Option<InstructionTicketAssociation>,
    action: StartAction,
}

#[derive(Debug)]
enum StartAction {
    Issue {
        entry: Box<Entry>,
        then: Box<Then>,
        closing: Option<Lsn>,
    },
    Retry {
        launch: Box<PreparedLaunch>,
    },
}

enum Awaiting {
    Start(Starting),
    End(Ending),
}

struct Ending {
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    baseline: InstructionBaselineRef,
    end: InstructionEndBoundary,
    recovery: bool,
    then: Box<Then>,
}

struct StartedRecovery {
    agent: AgentId,
    turn: TurnId,
    unissued: bool,
    end_lsn: Option<Lsn>,
    attempt: dudes_proto::instruction_state::InstructionAttempt,
}

struct TicketResolution {
    finish: UnknownFinish,
    then: Then,
}

struct UnknownFinish {
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    baseline: InstructionBaselineRef,
    reason: InstructionUnknownReason,
    coverage: Vec<InstructionGapKind>,
    end: Option<InstructionEndBoundary>,
    recovery: bool,
}

fn managed_owner(
    agent: AgentId,
    turn: TurnId,
    issue: InstructionIssueAssociation,
) -> InstructionObservationOwner {
    InstructionObservationOwner::Managed { agent, turn, issue }
}

fn issue_goal(record: &Record) -> Option<GoalId> {
    match record {
        Record::TurnIssued { goal, .. }
        | Record::TurnIssuedWithBrowserNote { goal, .. }
        | Record::NoteOnlyTurnIssued { goal, .. }
        | Record::TaughtNoteOnlyTurnIssued { goal, .. } => Some(*goal),
        Record::MailWakeOnlyTurnIssued(issue) => Some(issue.goal),
        _ => None,
    }
}

fn owner_agent(owner: InstructionObservationOwner) -> AgentId {
    match owner {
        InstructionObservationOwner::Managed { agent, .. }
        | InstructionObservationOwner::AttachedProcess { agent, .. }
        | InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
    }
}

fn gap(reason: InstructionGapKind, agent: AgentId) -> InstructionCoverageGap {
    InstructionCoverageGap {
        origin: dudes_proto::InstructionGapOrigin::Observed,
        reason,
        context: InstructionPathContext::Unresolved,
        agent: Some(agent),
        path: None,
    }
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Whether this agent owes a managed post-turn check deferred by instruction observation.
    #[must_use]
    pub fn pending_instruction_check(&self, agent: AgentId) -> bool {
        let judges = |owner, then: &Then| {
            matches!(owner, InstructionObservationOwner::Managed { agent: who, .. } if who == agent)
                && matches!(then, Then::Judge { agent: who, goal } if *who == agent
                    && self.world.goals.get(goal).is_some_and(|row| !row.cleared))
        };
        self.instructions.awaiting.values().any(|awaiting| {
            matches!(awaiting, Awaiting::End(ending) if judges(ending.owner, &ending.then))
        }) || self.ledger.values().any(|parked| {
            matches!((&parked.entry.record, &parked.then),
                (Record::InstructionObservationFinished { owner, .. },
                 Then::InstructionCompared { baseline, then })
                    if self.instructions.ending.contains(baseline) && judges(*owner, then))
        }) || self.instructions.recovery_checks.iter().any(|check| {
            check.agent == agent && self.recovery_check_due(*check)
        }) || matches!(self.instructions.recovery, Recovery::Checking(goal)
            if self.world.goals.get(&goal).is_some_and(|row| row.agent == agent))
    }

    pub(super) fn instruction_journaled(&mut self, lsn: Lsn, entry: &Entry) {
        if lsn == Lsn::ZERO
            || self.world.last_applied_lsn.is_some_and(|last| lsn <= last)
            || !matches!(
                entry.record,
                Record::InstructionObservationStarted { .. }
                    | Record::InstructionObservationFinished { .. }
                    | Record::TicketInstructionReviewed { .. }
                    | Record::InstructionAttemptBoundarySettled { .. }
                    | Record::PtyCloseRequested { .. }
                    | Record::PtyCloseCompleted { .. }
            )
        {
            return;
        }
        self.apply_entry(lsn, entry);
    }

    pub(super) fn recover_instruction_observations(&mut self) -> bool {
        match self.instructions.recovery {
            Recovery::Complete => return false,
            Recovery::Awaiting | Recovery::Checking(_) => return true,
            Recovery::NotStarted => {
                let ended: Vec<_> = self
                    .world
                    .active_folder_user_ids()
                    .filter_map(|agent| {
                        let row = self.world.agents.get(agent)?;
                        let turn = self.world.goals.get(&row.goal?)?.last_turn?;
                        self.world
                            .instructions
                            .has_attempt_for_turn(turn)
                            .then_some((*agent, turn))
                    })
                    .collect();
                for (agent, turn) in ended {
                    self.queue_recovery_check(agent, turn);
                }
                self.instructions.recovery = Recovery::Scanning;
            }
            Recovery::Scanning => {}
        }
        let next = self
            .world
            .instructions
            .unfinished()
            .find_map(|(_, attempt)| {
                let InstructionObservationOwner::Managed { agent, turn, issue } = attempt.owner
                else {
                    return None;
                };
                let end = self.world.instructions.managed_end_lsn(agent, turn)
                    .filter(|end| matches!(attempt.baseline, InstructionBaselineRef::Started { lsn } if *end > lsn));
                let unissued = issue == InstructionIssueAssociation::Prospective
                    && self.world.instructions.issue_lsn(turn).is_none();
                (unissued || end.is_some() || attempt.spawn_lsn.is_none()).then_some(
                    StartedRecovery {
                        agent,
                        turn,
                        unissued,
                        end_lsn: end,
                        attempt: attempt.clone(),
                    },
                )
            });
        if let Some(next) = next {
            self.instructions.recovery = Recovery::Awaiting;
            self.recover_started_attempt(next);
            return true;
        }
        let attached = self
            .world
            .instructions
            .unfinished()
            .find_map(|(_, attempt)| {
                matches!(
                    attempt.owner,
                    InstructionObservationOwner::AttachedProcess { .. }
                )
                .then(|| attempt.clone())
            });
        if let Some(attempt) = attached {
            self.instructions.recovery = Recovery::Awaiting;
            self.recover_attached_process(attempt);
            return true;
        }
        let attached_end = self
            .world
            .unmatched_attached_turn_ends(1)
            .0
            .next()
            .map(|(end_lsn, _)| *end_lsn);
        if let Some(end_lsn) = attached_end {
            self.instructions.recovery = Recovery::Awaiting;
            self.attached_hook_ended(end_lsn, true, Then::InstructionRecoveryContinue);
            return true;
        }
        if let Some(finish) = self.next_legacy_recovery() {
            if let InstructionObservationOwner::Managed { agent, turn, .. } = finish.owner
                && finish.end.is_some()
            {
                self.queue_recovery_check(agent, turn);
            }
            self.instructions.recovery = Recovery::Awaiting;
            self.resolve_instruction_ticket(finish, Then::InstructionRecoveryContinue);
            return true;
        }
        while let Some(check) = self.instructions.recovery_checks.pop_first() {
            if !self.recovery_check_due(check) || self.protect_failures(check.agent) {
                continue;
            }
            self.instructions.recovery = Recovery::Checking(check.goal);
            self.judge(check.agent, check.goal);
            return true;
        }
        self.instructions.recovery = Recovery::Complete;
        false
    }

    fn recover_started_attempt(&mut self, recovered: StartedRecovery) {
        let StartedRecovery {
            agent,
            turn,
            unissued,
            end_lsn,
            attempt,
        } = recovered;
        if unissued {
            self.persist(
                Record::InstructionObservationFinished {
                    owner: attempt.owner,
                    ticket: attempt.ticket,
                    baseline: attempt.baseline,
                    settlement: InstructionSettlement::NoIssuedTurn,
                },
                Then::InstructionRecoveryContinue,
            );
        } else if let Some(end_lsn) = end_lsn {
            self.queue_recovery_check(agent, turn);
            let end = InstructionEndBoundary::ManagedTurn { turn, end_lsn };
            if let Some(snapshot) = attempt.snapshot {
                self.instructions.ending.insert(attempt.baseline);
                let row = self.world.turns.get(&turn);
                let goal = row.and_then(|row| row.goal);
                let selection = row
                    .and_then(|row| row.model_selection.clone())
                    .or_else(|| {
                        self.world
                            .agents
                            .get(&agent)
                            .and_then(|row| self.model_selection(row))
                    })
                    .unwrap_or_else(ModelSelection::provider_default);
                self.request_instruction_sample(
                    attempt.owner,
                    goal,
                    selection,
                    Some(Box::new(snapshot)),
                    Awaiting::End(Ending {
                        owner: attempt.owner,
                        ticket: attempt.ticket,
                        baseline: attempt.baseline,
                        end,
                        recovery: true,
                        then: Box::new(Then::InstructionRecoveryContinue),
                    }),
                );
            } else {
                self.persist_instruction_unknown(
                    UnknownFinish {
                        owner: attempt.owner,
                        ticket: attempt.ticket,
                        baseline: attempt.baseline,
                        reason: InstructionUnknownReason::MissingBaseline,
                        coverage: vec![InstructionGapKind::MissingBaseline],
                        end: Some(end),
                        recovery: true,
                    },
                    Then::InstructionRecoveryContinue,
                );
            }
        } else {
            self.persist_instruction_unknown(
                UnknownFinish {
                    owner: attempt.owner,
                    ticket: attempt.ticket,
                    baseline: attempt.baseline,
                    reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    coverage: vec![InstructionGapKind::UnrecordedLaunchBoundary],
                    end: None,
                    recovery: true,
                },
                Then::InstructionRecoveryContinue,
            );
        }
    }

    fn recover_attached_process(
        &mut self,
        attempt: dudes_proto::instruction_state::InstructionAttempt,
    ) {
        let InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } = attempt.owner
        else {
            return;
        };
        let end = self.world.instructions.attachment_end_lsn(attachment_lsn)
            .filter(|end| matches!(attempt.baseline, InstructionBaselineRef::Started { lsn } if *end > lsn))
            .map(|end_lsn| InstructionEndBoundary::AttachedProcess { attachment_lsn, end_lsn });
        if let (Some(end), Some(snapshot)) = (end, attempt.snapshot) {
            self.instructions.ending.insert(attempt.baseline);
            self.request_instruction_sample(
                attempt.owner,
                attempt.ticket.map(|ticket| ticket.goal),
                ModelSelection::provider_default(),
                Some(Box::new(snapshot)),
                Awaiting::End(Ending {
                    owner: attempt.owner,
                    ticket: attempt.ticket,
                    baseline: attempt.baseline,
                    end,
                    recovery: true,
                    then: Box::new(Then::InstructionRecoveryContinue),
                }),
            );
            return;
        }
        let (reason, coverage) = if end.is_some() {
            (
                InstructionUnknownReason::MissingBaseline,
                InstructionGapKind::MissingBaseline,
            )
        } else if attempt.spawn_lsn.is_some() {
            (
                InstructionUnknownReason::IncompleteCoverage,
                InstructionGapKind::EndingBoundaryMissing,
            )
        } else {
            (
                InstructionUnknownReason::UnrecordedLaunchBoundary,
                InstructionGapKind::UnrecordedLaunchBoundary,
            )
        };
        self.resolve_instruction_ticket(
            UnknownFinish {
                owner: attempt.owner,
                ticket: attempt.ticket,
                baseline: attempt.baseline,
                reason,
                coverage: vec![coverage],
                end,
                recovery: true,
            },
            Then::InstructionRecoveryContinue,
        );
    }

    fn queue_recovery_check(&mut self, agent: AgentId, turn: TurnId) {
        let Some(end_lsn) = self.world.instructions.managed_end_lsn(agent, turn) else {
            return;
        };
        if let Some((started, _)) = self.world.instructions.current_for_turn(turn)
            && end_lsn <= started
        {
            return;
        }
        let Some(goal) = self.world.turns.get(&turn).and_then(|row| row.goal) else {
            return;
        };
        let check = RecoveryCheck {
            agent,
            goal,
            turn,
            end_lsn,
        };
        if self.recovery_check_due(check) {
            self.instructions.recovery_checks.insert(check);
        }
    }

    fn recovery_check_due(&self, check: RecoveryCheck) -> bool {
        self.world.goals.get(&check.goal).is_some_and(|row| {
            !row.cleared
                && row.agent == check.agent
                && row.last_turn == Some(check.turn)
                && matches!(&row.goal.predicate, dudes_proto::Predicate::Command { .. })
        }) && self.world.completed_goal_lsn(check.goal).is_none()
            && self.world.agents.get(&check.agent).is_some_and(|row| {
                row.goal == Some(check.goal)
                    && row.open_turn.is_none()
                    && !row.retired
                    && !row.halted
                    && !row.paused
                    && row.automatic_hold.is_none()
            })
            && self
                .world
                .instructions
                .managed_end_lsn(check.agent, check.turn)
                == Some(check.end_lsn)
            && !self
                .world
                .observed_predicate_after(check.goal, check.turn, check.end_lsn)
    }

    fn recovery_ticket(&self, goal: GoalId) -> Option<InstructionTicketAssociation> {
        let recorded = self
            .world
            .instructions
            .recorded_association_for_goal(goal)?;
        let current = self
            .world
            .instructions
            .association_for_goal(goal, recorded.seq_at_start)?;
        (current == recorded).then_some(recorded)
    }

    fn next_legacy_recovery(&self) -> Option<UnknownFinish> {
        let candidates: BTreeSet<_> = self
            .world
            .active_folder_user_ids()
            .copied()
            .chain(self.world.open_turn_owner_ids().copied())
            .chain(self.world.instructions.unresolved_owners())
            .collect();
        candidates.into_iter().find_map(|agent| {
            let row = self.world.agents.get(&agent)?;
            if let Some(turn) = row.open_turn {
                let issue_lsn = self.world.instructions.issue_lsn(turn)?;
                if self.world.instructions.has_attempt_for_turn(turn)
                    || self.world.instructions.attempt(issue_lsn).is_some()
                    || self.world.instructions.end_lsn(turn).is_some()
                {
                    return None;
                }
                return Some(UnknownFinish {
                    owner: managed_owner(
                        agent,
                        turn,
                        InstructionIssueAssociation::Issued { issue_lsn },
                    ),
                    ticket: self
                        .world
                        .turns
                        .get(&turn)
                        .and_then(|row| row.goal)
                        .and_then(|goal| self.recovery_ticket(goal)),
                    baseline: InstructionBaselineRef::MissingIssueBoundary { turn, issue_lsn },
                    reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    coverage: vec![
                        InstructionGapKind::MissingBaseline,
                        InstructionGapKind::UnrecordedLaunchBoundary,
                    ],
                    end: None,
                    recovery: true,
                });
            }
            let goal = row.goal?;
            let goal_row = self.world.goals.get(&goal)?;
            if row.retired || goal_row.cleared || self.world.completed_goal_lsn(goal).is_some() {
                return None;
            }
            let ticket = self.recovery_ticket(goal)?;
            let turn = goal_row.last_turn?;
            if self.world.instructions.has_attempt_for_turn(turn) {
                return None;
            }
            let issue_lsn = self.world.instructions.issue_lsn(turn)?;
            let end_lsn = self.world.instructions.managed_end_lsn(agent, turn)?;
            if self.world.instructions.attempt(end_lsn).is_some() {
                return None;
            }
            Some(UnknownFinish {
                owner: managed_owner(
                    agent,
                    turn,
                    InstructionIssueAssociation::Issued { issue_lsn },
                ),
                ticket: Some(ticket),
                baseline: InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn },
                reason: InstructionUnknownReason::MissingBaseline,
                coverage: vec![InstructionGapKind::MissingBaseline],
                end: Some(InstructionEndBoundary::ManagedTurn { turn, end_lsn }),
                recovery: true,
            })
        })
    }

    /// Whether recovery currently awaits this exact goal's predicate completion.
    #[must_use]
    pub fn awaiting_recovered_predicate(&self, goal: GoalId) -> bool {
        self.instructions.recovering_check(goal)
    }

    pub(super) fn predicate_ended_without_verdict(&mut self, goal: GoalId) {
        if self.awaiting_recovered_predicate(goal) {
            self.instruction_recovery_continue();
        }
    }

    pub(super) fn instruction_recovery_continue(&mut self) {
        self.instructions.recovery = Recovery::Scanning;
        self.on_started();
    }

    fn resolve_instruction_ticket(&mut self, finish: UnknownFinish, then: Then) {
        let Some(association) = finish.ticket else {
            self.persist_instruction_unknown(finish, then);
            return;
        };
        self.instructions.next_request += 1;
        let request = InstructionRequest(self.instructions.next_request);
        self.instructions
            .recovery_tickets
            .insert(request, TicketResolution { finish, then });
        self.pending.push(Effect::ResolveInstructionRecoveryTicket {
            request,
            association,
        });
    }

    pub(super) fn instruction_recovery_ticket_resolved(
        &mut self,
        request: InstructionRequest,
        ticket: Option<InstructionTicketAssociation>,
    ) {
        let Some(finish) = self.instructions.recovery_tickets.get(&request) else {
            return;
        };
        if ticket.is_some() && ticket != finish.finish.ticket {
            return;
        }
        let Some(TicketResolution { mut finish, then }) =
            self.instructions.recovery_tickets.remove(&request)
        else {
            return;
        };
        finish.ticket = ticket;
        self.persist_instruction_unknown(finish, then);
    }

    pub(super) fn observe_attached_hook_end(&mut self, agent: AgentId, last_message: Option<&str>) {
        let Some(row) = self
            .world
            .agents
            .get(&agent)
            .filter(|row| row.attached && !row.retired)
        else {
            return;
        };
        let session = row.session.clone();
        let turn = TurnId::from_ulid(self.rng.ulid());
        self.persist(
            Record::TurnEnded {
                final_reply: None,
                elapsed_ms: None,
                session_usage: None,
                agent,
                turn,
                mode: Mode::Attached,
                provenance: Provenance::Observed,
                exit: ExitStatus::UNKNOWN,
                usage: None,
                model_usage: BTreeMap::new(),
                last_message: last_message.map(dudes_proto::ticket::summary_text),
                session,
                outcome: None,
            },
            Then::InstructionAttachedHookEnded,
        );
    }

    pub(super) fn attached_hook_ended(&mut self, end_lsn: Lsn, recovery: bool, then: Then) {
        let Some(captured) = self.world.unmatched_attached_turn_end(end_lsn).cloned() else {
            return;
        };
        let owner = captured.owner;
        let ticket = captured.ticket;
        let InstructionObservationOwner::AttachedTurn { turn, .. } = owner else {
            return;
        };
        self.resolve_instruction_ticket(
            UnknownFinish {
                owner,
                ticket,
                baseline: InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn },
                reason: InstructionUnknownReason::AttachedTurnStartMissing,
                coverage: vec![
                    InstructionGapKind::MissingBaseline,
                    InstructionGapKind::AttachedTurnStartMissing,
                ],
                end: Some(InstructionEndBoundary::AttachedTurn { turn, end_lsn }),
                recovery,
            },
            then,
        );
    }

    pub(super) fn instruction_attached_open_failed(
        &mut self,
        started_lsn: Lsn,
        cleanup_unconfirmed: bool,
    ) {
        let Some(attempt) = self.world.instructions.attempt(started_lsn) else {
            return;
        };
        if !matches!(
            attempt.owner,
            InstructionObservationOwner::AttachedProcess { .. }
        ) || attempt.finished.is_some()
            || attempt.spawn_lsn.is_some()
            || !self.instructions.ending.insert(attempt.baseline)
        {
            return;
        }
        let (reason, gap) = if cleanup_unconfirmed {
            (
                InstructionUnknownReason::CleanupUnconfirmed,
                InstructionGapKind::CleanupUnconfirmed,
            )
        } else {
            (
                InstructionUnknownReason::UnrecordedLaunchBoundary,
                InstructionGapKind::UnrecordedLaunchBoundary,
            )
        };
        let baseline = attempt.baseline;
        let finish = UnknownFinish {
            owner: attempt.owner,
            ticket: attempt.ticket,
            baseline,
            reason,
            coverage: vec![gap],
            end: None,
            recovery: false,
        };
        self.resolve_instruction_ticket(
            finish,
            Then::InstructionCompared {
                baseline,
                then: Box::new(Then::Fold),
            },
        );
    }

    pub(super) fn instruction_launch_cleanup_unconfirmed(&mut self, turn: TurnId) {
        let Some((_, attempt)) = self.world.instructions.current_for_turn(turn) else {
            return;
        };
        if attempt.finished.is_some() || !self.instructions.ending.insert(attempt.baseline) {
            return;
        }
        let owner = attempt.owner;
        let ticket = attempt.ticket;
        let baseline = attempt.baseline;
        self.persist_instruction_unknown(
            UnknownFinish {
                owner,
                ticket,
                baseline,
                reason: InstructionUnknownReason::CleanupUnconfirmed,
                coverage: vec![InstructionGapKind::CleanupUnconfirmed],
                end: None,
                recovery: false,
            },
            Then::InstructionCompared {
                baseline,
                then: Box::new(Then::Fold),
            },
        );
    }

    fn persist_instruction_unknown(&mut self, finish: UnknownFinish, then: Then) {
        let UnknownFinish {
            owner,
            ticket,
            baseline,
            reason,
            coverage,
            end,
            recovery,
        } = finish;
        self.persist(
            Record::InstructionObservationFinished {
                owner,
                ticket,
                baseline,
                settlement: InstructionSettlement::Unknown {
                    reason,
                    end,
                    sampled_at: None,
                    recovery,
                    comparison: InstructionComparison {
                        changes: Vec::new(),
                        gaps: coverage
                            .into_iter()
                            .map(|reason| gap(reason, owner_agent(owner)))
                            .collect(),
                        usage: InstructionSampleUsage::default(),
                    },
                },
            },
            then,
        );
    }

    /// Capture only recorded launch metadata and named path selectors, without task or credentials.
    #[must_use]
    pub fn instruction_source(&self, agent: AgentId) -> Option<InstructionSource> {
        let row = self.world.agents.get(&agent)?;
        let adapter = self.adapters.shared(row.provider)?;
        let cwd = match row.worktree {
            Some(lease) => &self.world.worktrees.get(&lease)?.path,
            None => &row.cwd,
        };
        if cwd.as_os_str().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
            return None;
        }
        let cwd = cwd.clone();
        let mut clipped = self.config.daemon_env.len() > MAX_INSTRUCTION_INPUTS;
        let environment = self
            .config
            .daemon_env
            .iter()
            .take(MAX_INSTRUCTION_INPUTS)
            .filter_map(|var| {
                let name = var.name().to_str()?;
                let selected = [
                    "HOME",
                    "USERPROFILE",
                    "CLAUDE_CONFIG_DIR",
                    "CODEX_HOME",
                    "PI_CODING_AGENT_DIR",
                ]
                .iter()
                .any(|selected| {
                    if cfg!(windows) {
                        name.eq_ignore_ascii_case(selected)
                    } else {
                        name == *selected
                    }
                });
                if !selected {
                    return None;
                }
                if var.expose_value().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
                    clipped = true;
                    return None;
                }
                Some(var.clone())
            })
            .collect();
        Some(InstructionSource {
            agent,
            provider: row.provider,
            cwd,
            lease: row.worktree,
            mode: if row.attached {
                Mode::Attached
            } else {
                Mode::Managed
            },
            agent_dir: self.agent_dir(agent),
            adapter,
            environment,
            clipped,
            sandbox: row.sandbox,
            trusted: row.trusted,
            max_turns: row.max_turns,
            max_budget: row.max_budget,
        })
    }
    pub(super) fn no_host_attempt(
        &self,
        agent: AgentId,
        turn: Option<TurnId>,
    ) -> Option<NoHostAttempt> {
        let turn = turn?;
        let (started_lsn, attempt) = self.world.instructions.current_for_turn(turn)?;
        if !matches!(attempt.owner, InstructionObservationOwner::Managed { agent: captured_agent, turn: captured_turn, .. } if captured_agent == agent && captured_turn == turn)
            || attempt.finished.is_some()
            || attempt.spawn_lsn.is_some()
        {
            return None;
        }
        Some(NoHostAttempt {
            owner: attempt.owner,
            ticket: attempt.ticket,
            baseline: InstructionBaselineRef::Started { lsn: started_lsn },
        })
    }

    pub(super) fn observe_before_issue(
        &mut self,
        agent: AgentId,
        turn: TurnId,
        selection: ModelSelection,
        entry: Entry,
        then: Then,
    ) {
        if !self.instructions.pending_agents.insert(agent) {
            return;
        }
        let goal = issue_goal(&entry.record);
        let closing = (self.closing_active && matches!(entry.record, Record::TurnIssued { .. }))
            .then(|| {
                self.world
                    .closing
                    .as_ref()
                    .map(|session| session.announcement)
            })
            .flatten();
        let owner = managed_owner(agent, turn, InstructionIssueAssociation::Prospective);
        let starting = Starting {
            owner,
            goal,
            ticket: None,
            action: StartAction::Issue {
                entry: Box::new(entry),
                then: Box::new(then),
                closing,
            },
        };
        self.request_instruction_sample(owner, goal, selection, None, Awaiting::Start(starting));
    }

    pub(super) fn observe_retry(
        &mut self,
        agent: AgentId,
        turn: TurnId,
        launch: Box<PreparedLaunch>,
    ) {
        let Some(issue_lsn) = self.world.instructions.issue_lsn(turn) else {
            self.spawning.remove(&turn);
            return;
        };
        let goal = self.world.turns.get(&turn).and_then(|row| row.goal);
        let owner = managed_owner(
            agent,
            turn,
            InstructionIssueAssociation::Issued { issue_lsn },
        );
        let selection = launch.selection.clone();
        let starting = Starting {
            owner,
            goal,
            ticket: None,
            action: StartAction::Retry { launch },
        };
        self.request_instruction_sample(owner, goal, selection, None, Awaiting::Start(starting));
    }

    fn request_instruction_sample(
        &mut self,
        owner: InstructionObservationOwner,
        goal: Option<GoalId>,
        selection: ModelSelection,
        baseline: Option<Box<InstructionSnapshot>>,
        awaiting: Awaiting,
    ) {
        self.instructions.next_request += 1;
        let request = InstructionRequest(self.instructions.next_request);
        self.instructions.awaiting.insert(request, awaiting);
        self.pending.push(Effect::SampleInstructions {
            request,
            owner,
            goal,
            selection,
            baseline,
        });
    }

    pub(super) fn instruction_sampled(
        &mut self,
        request: InstructionRequest,
        snapshot: InstructionSnapshot,
        ticket: Option<InstructionTicketAssociation>,
        comparison: Option<InstructionComparison>,
    ) {
        let Some(awaiting) = self.instructions.awaiting.remove(&request) else {
            return;
        };
        match awaiting {
            Awaiting::Start(mut starting) => {
                starting.ticket = ticket.filter(|ticket| Some(ticket.goal) == starting.goal);
                let snapshot = if snapshot.validate().is_ok() {
                    snapshot
                } else {
                    InstructionSnapshot {
                        sampled_at: self.clock.now(),
                        source: InstructionSnapshotSource::Fresh,
                        execution_folder: None,
                        participants: Vec::new(),
                        inputs: Vec::new(),
                        inventoried: Vec::new(),
                        gaps: vec![gap(
                            InstructionGapKind::MetadataLimit,
                            owner_agent(starting.owner),
                        )],
                        usage: InstructionSampleUsage::default(),
                        boot: None,
                    }
                };
                self.persist(
                    Record::InstructionObservationStarted {
                        owner: starting.owner,
                        ticket: starting.ticket,
                        snapshot,
                    },
                    Then::InstructionBaselineSaved {
                        start: Box::new(starting),
                    },
                );
            }
            Awaiting::End(ending) => {
                let comparison = comparison
                    .filter(|comparison| comparison.validate().is_ok())
                    .unwrap_or_else(|| InstructionComparison {
                        changes: Vec::new(),
                        gaps: vec![gap(
                            InstructionGapKind::MetadataLimit,
                            owner_agent(ending.owner),
                        )],
                        usage: InstructionSampleUsage::default(),
                    });
                let settlement = InstructionSettlement::Compared {
                    end: ending.end,
                    sampled_at: snapshot.sampled_at,
                    recovery: ending.recovery,
                    comparison,
                };
                self.persist(
                    Record::InstructionObservationFinished {
                        owner: ending.owner,
                        ticket: ending.ticket,
                        baseline: ending.baseline,
                        settlement,
                    },
                    Then::InstructionCompared {
                        baseline: ending.baseline,
                        then: ending.then,
                    },
                );
            }
        }
    }

    pub(super) fn instruction_baseline_saved(&mut self, starting: Starting, lsn: Lsn) {
        let InstructionObservationOwner::Managed { agent, turn, .. } = starting.owner else {
            return;
        };
        match starting.action {
            StartAction::Issue {
                entry,
                then,
                closing,
            } => {
                let allowed = self.world.agents.get(&agent).is_some_and(|row| {
                    !row.retired
                        && !row.halted
                        && !row.paused
                        && !row.attached
                        && row.automatic_hold.is_none()
                        && row.open_turn.is_none()
                        && row.goal == starting.goal
                }) && starting
                    .goal
                    .is_none_or(|goal| !self.world.instructions.pending_for_goal(goal))
                    && !self.instructions.recovering()
                    && !self.closing_recovery
                    && !self.protection_pending(agent)
                    && self.world.mail_wake_history_valid()
                    && self.delayed_issue_admitted(agent, &entry, closing);
                if !allowed {
                    self.instructions.pending_agents.remove(&agent);
                    self.persist(
                        Record::InstructionObservationFinished {
                            owner: starting.owner,
                            ticket: starting.ticket,
                            baseline: InstructionBaselineRef::Started { lsn },
                            settlement: InstructionSettlement::NoIssuedTurn,
                        },
                        Then::Decide { agent },
                    );
                    return;
                }
                self.instructions.ready_turns.insert(turn);
                let Entry { at, record } = *entry;
                let at = if matches!(&record, Record::MailWakeOnlyTurnIssued(_)) {
                    at
                } else {
                    self.clock.now()
                };
                self.persist_unobserved(at, record, *then);
            }
            StartAction::Retry { launch } => {
                self.spawn_prepared(agent, turn, launch);
            }
        }
    }

    fn delayed_issue_admitted(&self, agent: AgentId, entry: &Entry, closing: Option<Lsn>) -> bool {
        match &entry.record {
            Record::TurnIssued { task, answers, .. } => match closing {
                Some(announcement) => {
                    self.delayed_saving_admitted(agent, announcement, task, answers)
                }
                None => self.delayed_ordinary_admitted(agent, None, task, answers),
            },
            Record::TurnIssuedWithBrowserNote {
                note_lsn,
                task,
                answers,
                ..
            } => self.delayed_ordinary_admitted(agent, Some(*note_lsn), task, answers),
            Record::ConversationTurnIssued {
                note_lsn,
                context_lsn,
                task,
                ..
            } => self.conversation_input_for(agent).is_some_and(|input| {
                input.note_lsn == *note_lsn
                    && input.context_lsn == *context_lsn
                    && matches!(dudes_policy::admit_conversation(&input).decision,
                        dudes_proto::NoteAdmission::Issue { task: current } if current == *task)
            }),
            Record::NoteOnlyTurnIssued {
                goal,
                note_lsn,
                task,
                ..
            }
            | Record::TaughtNoteOnlyTurnIssued {
                goal,
                note_lsn,
                task,
                ..
            } => self.completed_note_input_for(agent).is_some_and(|input| {
                input.goal() == *goal
                    && input.note_lsn() == *note_lsn
                    && match &entry.record {
                        Record::TaughtNoteOnlyTurnIssued { completion_lsn, .. } => {
                            input.completion_lsn() == *completion_lsn
                        }
                        _ => true,
                    }
                    && matches!(dudes_policy::admit_completed_note(&input).decision,
                        dudes_proto::NoteAdmission::Issue { task: current } if current == *task)
            }),
            Record::MailWakeOnlyTurnIssued(issue) => {
                self.delayed_mail_wake_admitted(issue, entry.at, self.clock.now())
            }
            _ => false,
        }
    }

    fn delayed_ordinary_admitted(
        &self,
        agent: AgentId,
        note_lsn: Option<Lsn>,
        task: &str,
        answers: &[Lsn],
    ) -> bool {
        if self.closing_active || self.closing_recovery {
            return false;
        }
        let Some(input) = self.loop_input_for(agent) else {
            return false;
        };
        if self
            .world
            .agents
            .get(&agent)
            .and_then(|row| row.goal)
            .is_some_and(|goal| self.goals_awaiting_board_result.contains(&goal))
        {
            return false;
        }
        let note = self.eligible_ordinary_note(agent);
        if note.as_ref().map(dudes_policy::BrowserNoteInput::note_lsn) != note_lsn {
            return false;
        }
        let decided = match note {
            Some(note) => dudes_policy::decide_with_browser_note(&input, &note),
            None => dudes_policy::decide(&input),
        };
        matches!(&decided.decision, LoopDecision::Continue { task: current } if current.as_str() == task)
            && decided.answers.as_slice() == answers
    }

    fn delayed_saving_admitted(
        &self,
        agent: AgentId,
        announcement: Lsn,
        task: &str,
        answers: &[Lsn],
    ) -> bool {
        let Some(session) = self.world.closing.as_ref().filter(|session| {
            self.closing_active && !session.is_complete() && session.announcement == announcement
        }) else {
            return false;
        };
        let Some(participant) = session.participants.get(&agent) else {
            return false;
        };
        if self.world.closing_standing(agent, participant) != crate::closing::Standing::Open
            || participant.mode != Mode::Managed
            || participant.acknowledged.is_some()
            || participant.saving_turn.is_some()
        {
            return false;
        }
        let Some(input) = self.loop_input_for(agent) else {
            return false;
        };
        let Some(goal) = self.world.agents.get(&agent).and_then(|row| row.goal) else {
            return false;
        };
        let input = dudes_policy::closing_input(announcement, agent, goal, input);
        let dudes_policy::ClosingDecided::Policy(decided) = dudes_policy::decide_closing(&input)
        else {
            return false;
        };
        matches!(&decided.decision, LoopDecision::Continue { task: current } if current.as_str() == task)
            && decided.answers.as_slice() == answers
    }

    pub(super) fn observe_after_end(&mut self, turn: TurnId, then: Then, end_lsn: Lsn) {
        let Some(row) = self.world.turns.get(&turn).cloned() else {
            self.resume_after(then, end_lsn);
            return;
        };
        let Some((started_lsn, attempt)) = self.world.instructions.current_for_turn(turn) else {
            self.resume_after(then, end_lsn);
            return;
        };
        let Some(snapshot) = attempt.snapshot.clone() else {
            self.resume_after(then, end_lsn);
            return;
        };
        let owner = attempt.owner;
        let ticket = attempt.ticket;
        let baseline = InstructionBaselineRef::Started { lsn: started_lsn };
        if !self.instructions.ending.insert(baseline) {
            return;
        }
        let selection = row
            .model_selection
            .or_else(|| {
                self.world
                    .agents
                    .get(&row.agent)
                    .and_then(|row| self.model_selection(row))
            })
            .unwrap_or_else(ModelSelection::provider_default);
        let ending = Ending {
            owner,
            ticket,
            baseline,
            end: InstructionEndBoundary::ManagedTurn { turn, end_lsn },
            recovery: false,
            then: Box::new(then),
        };
        self.request_instruction_sample(
            owner,
            row.goal,
            selection,
            Some(Box::new(snapshot)),
            Awaiting::End(ending),
        );
    }

    /// Pure declaration from the actual adapter composition and supplied execution folder.
    #[must_use]
    pub fn instruction_declaration(
        &self,
        agent: AgentId,
        cwd: &Path,
        mode: Mode,
        selection: &ModelSelection,
    ) -> Option<dudes_providers::instructions::InstructionDeclaration> {
        let row = self.world.agents.get(&agent)?;
        let adapter = self.adapters.get(row.provider)?;
        let agent_dir = self.agent_dir(agent);
        let ctx = AgentCtx {
            agent,
            cwd,
            agent_dir: &agent_dir,
            daemon_env: &self.config.daemon_env,
            sandbox: row.sandbox,
            trusted: row.trusted,
            max_turns: row.max_turns,
            max_budget: row.max_budget,
            mcp_servers: &[],
        };
        Some(adapter.instruction_manifest(&ctx, mode, selection))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        Actor, Command, EventKind, HookDecision, HookId, Input, SpendView, Timestamp,
    };
    use super::*;
    use crate::testing::Harness;
    use dudes_proto::InstructionNoProcessProof;

    fn acknowledge_actual_persist(
        harness: &mut Harness,
        effects: &[crate::testing::Seen],
    ) -> Vec<crate::testing::Seen> {
        let (token, entry) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry } => Some((*token, entry.clone())),
                _ => None,
            })
            .expect("the actual supervisor must request this durable boundary");
        let lsn = Lsn(harness.supervisor.world().last_applied_lsn.unwrap().0 + 1);
        harness.journal.push(entry);
        harness.step(Input::Persisted { token, lsn })
    }

    fn delayed_board_sample() -> (
        Harness,
        AgentId,
        GoalId,
        dudes_types::CardId,
        InstructionRequest,
        TurnId,
    ) {
        let mut harness = Harness::new();
        let agent = harness.hire();
        let card = dudes_types::CardId::from_parts(97, 1);
        let by = dudes_proto::Actor::Peer { pid: 77, uid: None };
        let entry = Entry {
            at: harness.supervisor.clock.now(),
            record: Record::CardCreated {
                card,
                title: "observable completion".into(),
                department: "infra".into(),
                by,
                deps: Vec::new(),
            },
        };
        let lsn = Lsn(harness.supervisor.world().last_applied_lsn.unwrap().0 + 1);
        harness.journal.push(entry.clone());
        assert!(harness.step(Input::BoardChanged { lsn, entry }).is_empty());
        let effects = harness.step(Input::Command(Command::GoalSet {
            agent,
            text: "immutable allocated task".into(),
            predicate: dudes_proto::Predicate::BoardState {
                card,
                status: dudes_types::CardStatus::Done,
            },
            budget: dudes_proto::GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 8,
            baseline: None,
            completion_check: None,
            browser_goal: None,
            goal_replacement: None,
        }));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let (request, turn) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::SampleInstructions {
                    request,
                    owner:
                        InstructionObservationOwner::Managed {
                            turn,
                            issue: InstructionIssueAssociation::Prospective,
                            ..
                        },
                    ..
                } => Some((*request, *turn)),
                _ => None,
            })
            .expect("the real Continue path must be waiting on its actual starting sample");
        assert!(
            harness
                .supervisor
                .instructions
                .pending_agents
                .contains(&agent)
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .issue_lsn(turn)
                .is_none()
        );
        (harness, agent, goal, card, request, turn)
    }

    fn deliver_delayed_start(
        harness: &mut Harness,
        request: InstructionRequest,
    ) -> Vec<crate::testing::Seen> {
        let Some(Awaiting::Start(starting)) =
            harness.supervisor.instructions.awaiting.get(&request)
        else {
            panic!("the actual starting sample is still outstanding");
        };
        let agent = owner_agent(starting.owner);
        let snapshot = InstructionSnapshot {
            sampled_at: harness.supervisor.clock.now(),
            source: InstructionSnapshotSource::Fresh,
            execution_folder: None,
            participants: Vec::new(),
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: vec![
                gap(InstructionGapKind::PathUnresolved, agent),
                gap(InstructionGapKind::DeclarationMissing, agent),
            ],
            usage: InstructionSampleUsage::default(),
            boot: None,
        };
        let effects = harness.step(Input::InstructionSampled {
            request,
            snapshot: Box::new(snapshot),
            ticket: None,
            comparison: None,
        });
        assert!(
            effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(record, Record::InstructionObservationStarted { .. }))
        );
        acknowledge_actual_persist(harness, &effects)
    }

    #[test]
    fn delayed_ordinary_baseline_cannot_issue_after_observed_completion() {
        check_delayed_board_readmission(true);
    }

    #[test]
    fn delayed_ordinary_baseline_cannot_issue_while_board_result_is_pending() {
        check_delayed_board_readmission(false);
    }

    fn actual_pending_request(effects: &[crate::testing::Seen]) -> InstructionRequest {
        effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::SampleInstructions {
                    request,
                    baseline: None,
                    ..
                } => Some(*request),
                _ => None,
            })
            .expect("the actual issue must reach its starting sample")
    }

    fn assert_exact_delayed_issue(harness: &mut Harness, request: InstructionRequest) {
        let Some(Awaiting::Start(Starting {
            action: StartAction::Issue { entry, .. },
            ..
        })) = harness.supervisor.instructions.awaiting.get(&request)
        else {
            panic!("the unissued intent remains captured");
        };
        let frozen = entry.as_ref().clone();
        let emitted_at =
            Timestamp::from_unix_millis(harness.supervisor.clock.peek().unix_millis() + 1_000);
        harness.supervisor.clock = crate::testing::FakeClock::frozen(emitted_at.unix_millis());
        let effects = deliver_delayed_start(harness, request);
        let issued = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { entry, .. } => Some(entry),
                _ => None,
            })
            .expect("the admitted prospective issue must request persistence");
        assert_eq!(issued.record, frozen.record);
        assert_eq!(issued.at, emitted_at);
        assert!(issued.at > frozen.at);
    }

    fn browser_attribution() -> dudes_proto::BrowserNoteAttribution {
        dudes_proto::BrowserNoteAttribution {
            request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
            request_digest: dudes_proto::record::Hex32::new([2; 32]),
            session_digest: dudes_proto::record::Hex32::new([3; 32]),
        }
    }

    #[test]
    fn an_unchanged_ordinary_issue_keeps_its_exact_allocated_record_after_sampling() {
        let (mut harness, _, _, _, request, _) = delayed_board_sample();
        assert_exact_delayed_issue(&mut harness, request);
    }

    #[test]
    fn an_unchanged_conversation_issue_keeps_its_exact_note_and_context_after_sampling() {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.step(Input::Spend {
            agent,
            view: SpendView::default(),
        });
        let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
        let effects = harness.step(Input::Command(Command::ConversationMessageQueued {
            agent,
            text: "whole finite message".into(),
            browser: browser_attribution(),
            context_lsn,
            context_reset_turn: None,
        }));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let request = actual_pending_request(&effects);
        assert_exact_delayed_issue(&mut harness, request);
    }

    #[test]
    fn an_unchanged_ordinary_browser_issue_keeps_its_exact_note_after_sampling() {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.drive(Input::Command(Command::Pause {
            agent,
            browser: None,
        }));
        let goal = harness.set_goal(agent, &["/usr/bin/false"]);
        harness.drive(Input::Command(Command::BrowserNoteQueued {
            agent,
            text: "whole browser note".into(),
            browser: browser_attribution(),
            destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal },
        }));
        let effects = harness.step(Input::Command(Command::Resume {
            agent,
            browser: None,
        }));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let request = actual_pending_request(&effects);
        assert_exact_delayed_issue(&mut harness, request);
    }

    #[test]
    fn an_unchanged_saving_issue_keeps_its_exact_closing_intent_after_sampling() {
        let (mut harness, _, _, request) = actual_saving_sample(false);
        assert_exact_delayed_issue(&mut harness, request);
    }

    fn pending_saving_dispatch() -> (Harness, AgentId, TurnId) {
        let (mut harness, agent, _, request) = actual_saving_sample(false);
        let mut effects = deliver_delayed_start(&mut harness, request);
        for _ in 0..4 {
            if effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::Spawn { .. }))
            {
                break;
            }
            effects = acknowledge_actual_persist(&mut harness, &effects);
        }
        let turn = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Spawn { turn, .. } => Some(*turn),
                _ => None,
            })
            .expect("the saving issue reached its actual host dispatch");
        (harness, agent, turn)
    }

    #[test]
    fn closing_expiry_waits_for_the_claimed_saving_launch_to_fold() {
        let (mut harness, agent, turn) = pending_saving_dispatch();
        assert!(harness.supervisor.world().turns[&turn].is_issued_not_spawned());
        assert!(harness.supervisor.spawning.contains(&turn));
        assert!(harness.supervisor.ledger.is_empty());
        assert_eq!(
            harness.supervisor.closing_finish(true).unwrap_err(),
            crate::closing::ClosingError::LaunchPending
        );
        assert!(
            !harness
                .supervisor
                .world()
                .closing
                .as_ref()
                .unwrap()
                .is_complete()
        );
        assert!(harness.supervisor.effects().is_empty());
        let effects = harness.step(Input::Spawned {
            turn,
            key: crate::host::AdoptionKey {
                pid: 990,
                start_time: 123,
                boot: None,
            },
            mcp_grant: None,
        });
        acknowledge_actual_persist(&mut harness, &effects);
        assert_eq!(harness.supervisor.world().turns[&turn].agent, agent);
        assert!(harness.supervisor.world().turns[&turn].is_open_child());
        assert!(!harness.supervisor.spawning.contains(&turn));
        assert!(
            harness
                .supervisor
                .closing_finish(true)
                .unwrap()
                .iter()
                .any(|effect| {
                    matches!(
                        effect,
                        Effect::Persist {
                            entry: Entry {
                                record: Record::ClosingTime {
                                    phase: dudes_proto::ClosingPhase::Complete
                                },
                                ..
                            },
                            ..
                        }
                    )
                })
        );
    }

    #[test]
    fn closing_expiry_keeps_host_answered_cleanup_uncertainty_without_waiting_forever() {
        let (mut harness, agent, turn) = pending_saving_dispatch();
        let (started, _) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let effects = harness.step(Input::InstructionLaunchCleanupUnconfirmed { turn });
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .current_for_turn(turn)
                .is_some()
        );
        assert!(!harness.supervisor.ledger.is_empty());
        assert_eq!(
            harness.supervisor.closing_finish(true).unwrap_err(),
            crate::closing::ClosingError::LaunchPending
        );
        acknowledge_actual_persist(&mut harness, &effects);
        let world = harness.supervisor.world();
        let attempt = world.instructions.attempt(started).unwrap();
        assert!(matches!(
            attempt
                .finished
                .as_ref()
                .map(|finished| &finished.settlement),
            Some(InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::CleanupUnconfirmed,
                end: None,
                ..
            })
        ));
        assert_eq!(attempt.spawn_lsn, None);
        assert_eq!(attempt.boundary_settled, None);
        assert!(world.instructions.current_for_turn(turn).is_none());
        assert!(
            world
                .instructions
                .unresolved_launch_attempts(agent)
                .any(|(position, _)| *position == started)
        );
        assert!(world.turns[&turn].is_issued_not_spawned());
        assert!(harness.supervisor.spawning.contains(&turn));
        assert!(harness.supervisor.ledger.is_empty());
        assert!(
            harness
                .supervisor
                .closing_finish(true)
                .unwrap()
                .iter()
                .any(|effect| {
                    matches!(
                        effect,
                        Effect::Persist {
                            entry: Entry {
                                record: Record::ClosingTime {
                                    phase: dudes_proto::ClosingPhase::Complete
                                },
                                ..
                            },
                            ..
                        }
                    )
                })
        );
        assert!(harness.supervisor.spawning.contains(&turn));
    }

    fn held_end_sample(
        harness: &mut Harness,
        turn: TurnId,
    ) -> (InstructionRequest, Box<InstructionSnapshot>) {
        let effects = harness.step(exited(turn));
        let effects = acknowledge_actual_persist(harness, &effects);
        effects
            .into_iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::SampleInstructions {
                    request,
                    baseline: Some(snapshot),
                    ..
                } => Some((request, snapshot)),
                _ => None,
            })
            .expect("the ended managed turn samples its baseline")
    }

    fn finish_held_sample(
        harness: &mut Harness,
        request: InstructionRequest,
        snapshot: Box<InstructionSnapshot>,
    ) -> Vec<crate::testing::Seen> {
        harness.step(Input::InstructionSampled {
            request,
            snapshot,
            ticket: None,
            comparison: Some(Box::new(InstructionComparison {
                changes: Vec::new(),
                gaps: Vec::new(),
                usage: InstructionSampleUsage::default(),
            })),
        })
    }

    #[test]
    fn pending_instruction_check_owns_the_sample_and_finish_persist_until_judge_handoff() {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["/usr/bin/false"]);
        let other = harness.hire();
        let turn = harness.open_turn(agent).unwrap();
        assert!(!harness.supervisor.pending_instruction_check(agent));
        let (request, snapshot) = held_end_sample(&mut harness, turn);
        assert!(harness.supervisor.pending_instruction_check(agent));
        assert!(!harness.supervisor.pending_instruction_check(other));
        let effects = finish_held_sample(&mut harness, request, snapshot);
        assert!(harness.supervisor.pending_instruction_check(agent));
        assert!(!harness.supervisor.pending_instruction_check(other));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        assert_eq!(recovered_checks(&effects, goal), 1);
        assert!(!harness.supervisor.pending_instruction_check(agent));
    }

    #[test]
    fn clearing_a_goal_during_comparison_removes_its_pending_instruction_check() {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["/usr/bin/false"]);
        let turn = harness.open_turn(agent).unwrap();
        let (request, snapshot) = held_end_sample(&mut harness, turn);
        assert!(harness.supervisor.pending_instruction_check(agent));
        harness.drive(Input::Command(Command::GoalClear { agent, goal }));
        assert!(harness.supervisor.world().goals[&goal].cleared);
        assert!(!harness.supervisor.pending_instruction_check(agent));
        let effects = finish_held_sample(&mut harness, request, snapshot);
        assert!(!harness.supervisor.pending_instruction_check(agent));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        assert_eq!(recovered_checks(&effects, goal), 0);
        assert!(!harness.supervisor.pending_instruction_check(agent));
    }

    #[test]
    fn managed_notes_owe_no_pending_instruction_check() {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.drive(Input::Spend {
            agent,
            view: SpendView::default(),
        });
        harness.drive(Input::Command(Command::ConversationMessageQueued {
            agent,
            text: "read this finite note".into(),
            browser: browser_attribution(),
            context_lsn: harness.supervisor.world().agents[&agent].context_lsn,
            context_reset_turn: None,
        }));
        let turn = harness.open_turn(agent).expect("the note's managed turn");
        let (request, snapshot) = held_end_sample(&mut harness, turn);
        assert!(!harness.supervisor.pending_instruction_check(agent));
        let effects = finish_held_sample(&mut harness, request, snapshot);
        assert!(!harness.supervisor.pending_instruction_check(agent));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::RunPredicate { .. }))
        );
        assert!(!harness.supervisor.pending_instruction_check(agent));
    }

    #[test]
    fn pending_instruction_checks_include_due_recovery_queued_behind_another_agent() {
        for pause_queued in [false, true] {
            let mut live = Harness::new();
            let (first, first_goal) = live.hire_with_goal(&["/usr/bin/false"]);
            let first_turn = live.open_turn(first).unwrap();
            live.drive(exited(first_turn));
            let (second, second_goal) = live.hire_with_goal(&["/usr/bin/false"]);
            let second_turn = live.open_turn(second).unwrap();
            live.drive(exited(second_turn));
            let other = live.hire();
            let mut recovered = replay_prefix(&live.journal);
            let effects = recovered.step(Input::Started);
            assert_eq!(recovered_checks(&effects, first_goal), 1);
            assert_eq!(recovered_checks(&effects, second_goal), 0);
            assert!(recovered.supervisor.pending_instruction_check(first));
            assert!(recovered.supervisor.pending_instruction_check(second));
            assert!(!recovered.supervisor.pending_instruction_check(other));
            if pause_queued {
                recovered.drive(Input::Command(Command::Pause {
                    agent: second,
                    browser: None,
                }));
                assert!(!recovered.supervisor.pending_instruction_check(second));
            }
            let trace_start = recovered.trace.len();
            recovered.drive(observed_failure(first_goal));
            assert!(!recovered.supervisor.pending_instruction_check(first));
            assert_eq!(
                recovered_checks(&recovered.trace[trace_start..], second_goal),
                usize::from(!pause_queued)
            );
            assert_eq!(
                recovered.supervisor.pending_instruction_check(second),
                !pause_queued
            );
            if !pause_queued {
                recovered.drive(observed_failure(second_goal));
                assert!(!recovered.supervisor.pending_instruction_check(second));
            }
        }
    }

    fn actual_turn_end_to_check(harness: &mut Harness, turn: TurnId) {
        let effects = harness.step(exited(turn));
        let effects = acknowledge_actual_persist(harness, &effects);
        let request = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::SampleInstructions {
                    request,
                    baseline: Some(_),
                    ..
                } => Some(*request),
                _ => None,
            })
            .expect("the actual observed end must sample its recorded baseline");
        let snapshot = InstructionSnapshot {
            sampled_at: harness.supervisor.clock.now(),
            source: InstructionSnapshotSource::Fresh,
            execution_folder: None,
            participants: Vec::new(),
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: vec![gap(
                InstructionGapKind::PathUnresolved,
                harness.supervisor.world().turns[&turn].agent,
            )],
            usage: InstructionSampleUsage::default(),
            boot: None,
        };
        let effects = harness.step(Input::InstructionSampled {
            request,
            snapshot: Box::new(snapshot),
            ticket: None,
            comparison: Some(Box::new(InstructionComparison {
                changes: Vec::new(),
                gaps: vec![gap(
                    InstructionGapKind::DeclarationMissing,
                    harness.supervisor.world().turns[&turn].agent,
                )],
                usage: InstructionSampleUsage::default(),
            })),
        });
        let effects = acknowledge_actual_persist(harness, &effects);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::RunPredicate { .. }))
        );
    }

    fn actual_saving_sample(board: bool) -> (Harness, AgentId, GoalId, InstructionRequest) {
        let mut harness = Harness::new();
        let agent = harness.hire();
        let predicate = if board {
            dudes_proto::Predicate::BoardState {
                card: dudes_types::CardId::from_parts(97, 99),
                status: dudes_types::CardStatus::Done,
            }
        } else {
            dudes_proto::Predicate::Command {
                argv: vec!["/usr/bin/false".into()],
                timeout_s: 600,
            }
        };
        harness.drive(Input::Command(Command::GoalSet {
            agent,
            text: "save the same current work".into(),
            predicate,
            budget: dudes_proto::GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 8,
            baseline: None,
            completion_check: None,
            browser_goal: None,
            goal_replacement: None,
        }));
        let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
        let turn = harness.open_turn(agent).unwrap();
        let effects: Vec<_> = harness
            .supervisor
            .closing_announce()
            .unwrap()
            .iter()
            .map(crate::testing::Seen::of)
            .collect();
        acknowledge_actual_persist(&mut harness, &effects);
        actual_turn_end_to_check(&mut harness, turn);
        assert!(
            harness
                .supervisor
                .world()
                .closing
                .as_ref()
                .unwrap()
                .participants
                .contains_key(&agent)
        );
        let _ = harness.supervisor.take_effects();
        harness.supervisor.decide(agent);
        let effects: Vec<_> = harness
            .supervisor
            .effects()
            .iter()
            .map(crate::testing::Seen::of)
            .collect();
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let request = actual_pending_request(&effects);
        let Some(Awaiting::Start(Starting {
            action: StartAction::Issue {
                closing: Some(_), ..
            },
            ..
        })) = harness.supervisor.instructions.awaiting.get(&request)
        else {
            panic!("the actual saving issue must retain its announcement");
        };
        (harness, agent, goal, request)
    }

    #[test]
    fn a_saving_issue_remains_admitted_while_its_missing_board_result_is_pending() {
        let (mut harness, _, goal, request) = actual_saving_sample(true);
        assert!(
            harness
                .supervisor
                .goals_awaiting_board_result
                .contains(&goal)
        );
        assert_exact_delayed_issue(&mut harness, request);
    }

    #[test]
    fn a_completed_goal_browser_command_keeps_its_exact_completion_proof_after_sampling() {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
        let turn = harness.open_turn(agent).unwrap();
        harness.drive(exited(turn));
        harness.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["/usr/bin/true".into()],
                program: Some("/usr/bin/true".into()),
                exit: Some(0),
                output_tail: String::new(),
            },
        });
        assert!(harness.supervisor.world().completed_goal(agent).is_some());
        let effects = harness.step(Input::Command(Command::BrowserNoteQueued {
            agent,
            text: "whole completed-goal command".into(),
            browser: browser_attribution(),
            destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal },
        }));
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let effects = acknowledge_actual_persist(&mut harness, &effects);
        let request = actual_pending_request(&effects);
        let Some(Awaiting::Start(Starting {
            action: StartAction::Issue { entry, .. },
            ..
        })) = harness.supervisor.instructions.awaiting.get(&request)
        else {
            panic!("captured completed command");
        };
        assert!(matches!(
            entry.record,
            Record::TaughtNoteOnlyTurnIssued { .. }
        ));
        assert_exact_delayed_issue(&mut harness, request);
    }

    #[test]
    fn an_already_issued_retry_stays_owed_after_its_new_baseline_is_saved() {
        let (mut harness, agent, turn) = refused_attempt();
        let task = harness.supervisor.world().turns[&turn].task.clone();
        refuse(&mut harness, agent, turn);
        let mut effects = harness.step(Input::Command(Command::Resume {
            agent,
            browser: None,
        }));
        for _ in 0..8 {
            if effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::SampleInstructions { .. }))
            {
                break;
            }
            effects = acknowledge_actual_persist(&mut harness, &effects);
        }
        let request = actual_pending_request(&effects);
        let Some(Awaiting::Start(Starting {
            owner:
                InstructionObservationOwner::Managed {
                    turn: owned,
                    issue: InstructionIssueAssociation::Issued { .. },
                    ..
                },
            action: StartAction::Retry { launch },
            ..
        })) = harness.supervisor.instructions.awaiting.get(&request)
        else {
            panic!("the actual Resume path must sample an already-issued physical retry");
        };
        assert_eq!(*owned, turn);
        assert_eq!(launch.task, task);
        let mut effects = deliver_delayed_start(&mut harness, request);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(
                    record,
                    Record::TurnIssued { .. }
                        | Record::InstructionObservationFinished {
                            settlement: InstructionSettlement::NoIssuedTurn,
                            ..
                        }
                ))
        );
        assert_eq!(harness.supervisor.world().turns[&turn].task, task);
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );
        for _ in 0..8 {
            if effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::Spawn { .. }))
            {
                break;
            }
            effects = acknowledge_actual_persist(&mut harness, &effects);
        }
        assert!(effects.iter().any(|effect| matches!(effect,
            crate::testing::Seen::Spawn { turn: spawned, spec, .. }
                if *spawned == turn && spec.argv.iter().any(|arg| arg == &std::ffi::OsString::from(&task)))),
            "the saved retry baseline must still dispatch the immutable owed task");
    }

    fn check_delayed_board_readmission(passing_result: bool) {
        let (mut harness, agent, goal, card, request, turn) = delayed_board_sample();
        let entry = Entry {
            at: harness.supervisor.clock.now(),
            record: Record::CardTransition {
                card,
                from: dudes_types::CardStatus::Todo,
                to: dudes_types::CardStatus::Done,
                by: dudes_proto::Actor::Peer { pid: 77, uid: None },
                expected_seq: dudes_types::Seq::ZERO,
                reason: None,
            },
        };
        let lsn = Lsn(harness.supervisor.world().last_applied_lsn.unwrap().0 + 1);
        harness.journal.push(entry.clone());
        let effects = harness.step(Input::BoardChanged { lsn, entry });
        assert!(effects.iter().any(|effect| matches!(effect,
            crate::testing::Seen::RunPredicate { goal: checked } if *checked == goal)));
        assert!(
            harness
                .supervisor
                .goals_awaiting_board_result
                .contains(&goal)
        );
        if passing_result {
            let effects = harness.step(Input::PredicateDone {
                goal,
                pass: true,
                evidence: dudes_proto::Evidence::Card {
                    card,
                    status: dudes_types::CardStatus::Done,
                    by: dudes_proto::Actor::Peer { pid: 77, uid: None },
                },
            });
            acknowledge_actual_persist(&mut harness, &effects);
            let input = harness.supervisor.loop_input_for(agent).unwrap();
            assert!(matches!(
                dudes_policy::decide(&input).decision,
                dudes_proto::LoopDecision::Done
            ));
        }
        let effects = deliver_delayed_start(&mut harness, request);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(record,
            Record::TurnIssued { turn: issued, .. } if *issued == turn)),
            "the delayed ordinary baseline must not admit the stale allocated issue"
        );
        assert!(!harness.trace.iter().any(|effect| matches!(effect,
            crate::testing::Seen::Spawn { turn: spawned, .. } if *spawned == turn)));
        assert!(effects.iter().filter_map(crate::testing::Seen::record).any(|record| matches!(record,
            Record::InstructionObservationFinished { owner: InstructionObservationOwner::Managed { turn: owned, .. },
                settlement: InstructionSettlement::NoIssuedTurn, .. } if *owned == turn)));
        acknowledge_actual_persist(&mut harness, &effects);
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .issue_lsn(turn)
                .is_none()
        );
    }

    fn exited(turn: TurnId) -> Input {
        Input::ChildExited {
            turn,
            elapsed_ms: None,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
            final_reply: None,
        }
    }

    fn replay_prefix(entries: &[Entry]) -> Harness {
        let mut harness = Harness::new();
        for (lsn, entry) in (1u64..).zip(entries) {
            harness.step(Input::Replayed {
                lsn: Lsn(lsn),
                entry: entry.clone(),
            });
        }
        harness
    }

    fn ended_legacy_ticket() -> (Vec<Entry>, AgentId, TurnId, GoalId) {
        let (mut entries, agent, turn) =
            crate::testing::journal_with_issued_turn(None, "unchanged task");
        let Record::GoalSet { goal, .. } = &entries[1].record else {
            panic!("goal fixture")
        };
        let goal = goal.clone();
        let goal_id = goal.id;
        let card = dudes_types::CardId::from_parts(71, 2);
        entries[1].record = Record::TicketStarted {
            card,
            agent,
            goal,
            expected_seq: dudes_types::Seq(9),
        };
        entries.insert(
            1,
            Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::TicketAssigned {
                    card,
                    agent,
                    by: dudes_proto::Actor::Peer { pid: 77, uid: None },
                    message: None,
                    expected_seq: dudes_types::Seq(8),
                },
            },
        );
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::TurnEnded {
                agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                elapsed_ms: None,
                exit: dudes_proto::ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                session_usage: None,
                model_usage: BTreeMap::new(),
                last_message: None,
                final_reply: None,
                session: None,
                outcome: Some(dudes_proto::TurnOutcome::Completed),
            },
        });
        (entries, agent, turn, goal_id)
    }

    fn append_attached(entries: &mut Vec<Entry>, agent: AgentId) -> Lsn {
        let lsn = Lsn(u64::try_from(entries.len()).unwrap() + 1);
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Attached {
                agent,
                by: Actor::System,
                provenance: None,
                hook_port: None,
            },
        });
        lsn
    }

    fn append_attached_baseline(entries: &mut Vec<Entry>, agent: AgentId, attachment_lsn: Lsn) {
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::InstructionObservationStarted {
                owner: InstructionObservationOwner::AttachedProcess {
                    agent,
                    attachment_lsn,
                },
                ticket: None,
                snapshot: InstructionSnapshot {
                    sampled_at: Timestamp::UNIX_EPOCH,
                    source: InstructionSnapshotSource::Fresh,
                    execution_folder: None,
                    participants: Vec::new(),
                    inputs: Vec::new(),
                    inventoried: Vec::new(),
                    gaps: vec![gap(InstructionGapKind::PathUnresolved, agent)],
                    usage: InstructionSampleUsage::default(),
                    boot: None,
                },
            },
        });
    }

    fn assert_instruction_hold_denies_hooks(harness: &Harness, agent: AgentId) {
        for kind in [
            EventKind::PromptSubmit,
            EventKind::PreTool {
                name: "owned tool".into(),
                input: serde_json::Value::Null,
            },
        ] {
            assert!(matches!(
                harness
                    .supervisor
                    .hook_reply_without_write(agent, &kind)
                    .unwrap()
                    .decision,
                HookDecision::Deny { .. }
            ));
        }
    }

    #[test]
    fn an_observed_attached_stop_records_a_missing_turn_start_without_finishing_the_process() {
        let (mut entries, agent, old_turn, goal) = ended_legacy_ticket();
        let attachment_lsn = append_attached(&mut entries, agent);
        let starting_lsn = Lsn(u64::try_from(entries.len()).unwrap() + 1);
        append_attached_baseline(&mut entries, agent, attachment_lsn);
        let mut harness = replay_prefix(&entries);
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .current_for_attachment(attachment_lsn)
                .is_some()
        );
        assert!(matches!(
            harness
                .supervisor
                .hook_reply_without_write(agent, &EventKind::PromptSubmit)
                .unwrap()
                .decision,
            HookDecision::Allow
        ));
        let event = dudes_providers::claude::translate(agent, Timestamp::UNIX_EPOCH,
            &serde_json::json!({"hook_event_name":"Stop","last_assistant_message":"owned observed end"})).unwrap();
        let mut queue = std::collections::VecDeque::from(harness.step(Input::Hook {
            hook: HookId(91),
            event,
        }));
        let mut position = u64::try_from(entries.len()).unwrap();
        let mut ended = None;
        let mut finished = None;
        while let Some(effect) = queue.pop_front() {
            if let crate::testing::Seen::ResolveInstructionRecoveryTicket {
                request,
                association,
            } = effect
            {
                queue.extend(harness.step(Input::InstructionRecoveryTicketResolved {
                    request,
                    ticket: Some(association),
                }));
                continue;
            }
            if let crate::testing::Seen::Persist { token, entry } = effect {
                position += 1;
                match &entry.record {
                    Record::TurnEnded {
                        turn,
                        mode: Mode::Attached,
                        provenance: Provenance::Observed,
                        ..
                    } => ended = Some((*turn, Lsn(position))),
                    Record::InstructionObservationFinished {
                        owner: InstructionObservationOwner::AttachedTurn { turn, .. },
                        baseline: InstructionBaselineRef::MissingTurnBoundary { end_lsn, .. },
                        settlement:
                            InstructionSettlement::Unknown {
                                reason: InstructionUnknownReason::AttachedTurnStartMissing,
                                sampled_at: None,
                                end: Some(InstructionEndBoundary::AttachedTurn { .. }),
                                ..
                            },
                        ..
                    } => finished = Some((*turn, *end_lsn)),
                    Record::TurnIssued { .. } => {
                        panic!("an observed hook end invents no managed issue")
                    }
                    _ => {}
                }
                queue.extend(harness.step(Input::Persisted {
                    token,
                    lsn: Lsn(position),
                }));
            }
        }
        let ended = ended.expect("the actual translated Stop must have an observed end receipt");
        assert_ne!(ended.0, old_turn);
        assert_eq!(finished, Some(ended));
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .current_for_attachment(attachment_lsn)
                .is_some_and(|(lsn, attempt)| lsn == starting_lsn && attempt.finished.is_none())
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .pending_for_goal(goal)
        );
        assert_instruction_hold_denies_hooks(&harness, agent);
        assert!(!harness.supervisor.pending_instruction_check(agent));
    }

    #[test]
    fn recovery_finishes_the_actual_attached_hook_end_after_its_continuation_was_lost() {
        let (mut entries, agent, old_turn, goal) = ended_legacy_ticket();
        append_attached(&mut entries, agent);
        let mut live = replay_prefix(&entries);
        let event = dudes_providers::claude::translate(
            agent,
            Timestamp::UNIX_EPOCH,
            &serde_json::json!({"hook_event_name":"Stop"}),
        )
        .unwrap();
        let observed = live.step(Input::Hook {
            hook: HookId(92),
            event,
        });
        let end = observed
            .iter()
            .find_map(|seen| match seen {
                crate::testing::Seen::Persist { entry, .. }
                    if matches!(
                        entry.record,
                        Record::TurnEnded {
                            mode: Mode::Attached,
                            ..
                        }
                    ) =>
                {
                    Some(entry.clone())
                }
                _ => None,
            })
            .unwrap();
        let Record::TurnEnded { turn, .. } = end.record else {
            panic!("actual hook end");
        };
        assert_ne!(turn, old_turn);
        entries.push(end);
        let end_lsn = Lsn(u64::try_from(entries.len()).unwrap());
        let mut recovered = replay_prefix(&entries);
        let captured = recovered
            .supervisor
            .world()
            .unmatched_attached_turn_end(end_lsn)
            .unwrap();
        assert_eq!(captured.goal, Some(goal));
        let mut queue = std::collections::VecDeque::from(recovered.step(Input::Started));
        let mut finished = None;
        while let Some(seen) = queue.pop_front() {
            match seen {
                crate::testing::Seen::ResolveInstructionRecoveryTicket {
                    request,
                    association,
                } => {
                    queue.extend(recovered.step(Input::InstructionRecoveryTicketResolved {
                        request,
                        ticket: Some(association),
                    }));
                }
                crate::testing::Seen::Persist { token, entry } => {
                    if matches!(&entry.record, Record::InstructionObservationFinished {
                        owner: InstructionObservationOwner::AttachedTurn { turn: owned, attachment_lsn: None, .. },
                        baseline: InstructionBaselineRef::MissingTurnBoundary { end_lsn: ended, .. },
                        settlement: InstructionSettlement::Unknown {
                            reason: InstructionUnknownReason::AttachedTurnStartMissing,
                            sampled_at: None, recovery: true, .. }, ..
                    } if *owned == turn && *ended == end_lsn)
                    {
                        finished = Some(entry.clone());
                    }
                    entries.push(entry);
                    queue.extend(recovered.step(Input::Persisted {
                        token,
                        lsn: Lsn(u64::try_from(entries.len()).unwrap()),
                    }));
                }
                _ => {}
            }
        }
        assert!(
            finished.is_some(),
            "the fsynced actual Stop must recover its missing-start evidence"
        );
        assert!(
            recovered
                .supervisor
                .world()
                .unmatched_attached_turn_end(end_lsn)
                .is_none()
        );
        let mut again = replay_prefix(&entries);
        let effects = again.step(Input::Started);
        assert!(!effects.iter().filter_map(crate::testing::Seen::record).any(|record|
            matches!(record, Record::InstructionObservationFinished {
                owner: InstructionObservationOwner::AttachedTurn { turn: owned, .. }, .. } if *owned == turn)));
        assert!(
            again
                .supervisor
                .world()
                .unmatched_attached_turn_end(end_lsn)
                .is_none()
        );
    }

    #[test]
    fn recovery_preserves_an_attached_native_receipt_without_inventing_a_close() {
        let (mut entries, agent, _, _) = ended_legacy_ticket();
        let attachment_lsn = append_attached(&mut entries, agent);
        let started_lsn = Lsn(u64::try_from(entries.len()).unwrap() + 1);
        let owner = InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn,
        };
        append_attached_baseline(&mut entries, agent, attachment_lsn);
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::PtySpawned {
                agent,
                pty: dudes_proto::rpc::pty::PtyId(7),
                pid: 44001,
                start_time: 91,
                boot: None,
            },
        });
        let mut harness = replay_prefix(&entries);
        assert!(
            harness
                .supervisor
                .world()
                .attached_native_spawn_receipt(started_lsn)
                .is_some()
        );
        let effects = harness.step(Input::Started);
        let (token, entry) = effects.iter().find_map(|effect| match effect {
            crate::testing::Seen::Persist { token, entry }
                if matches!(entry.record, Record::InstructionObservationFinished { owner: found, .. } if found == owner) =>
                Some((*token, entry.clone())),
            _ => None,
        }).expect("the recorded open PTY needs ending-boundary uncertainty on recovery");
        assert!(
            matches!(&entry.record, Record::InstructionObservationFinished {
            baseline: InstructionBaselineRef::Started { lsn },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::IncompleteCoverage, end: None,
                sampled_at: None, recovery: true, comparison,
            }, ..
        } if *lsn == started_lsn && comparison.changes.is_empty()
            && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::EndingBoundaryMissing)
            && comparison.usage == InstructionSampleUsage::default())
        );
        entries.push(entry);
        harness.step(Input::Persisted {
            token,
            lsn: Lsn(u64::try_from(entries.len()).unwrap()),
        });
        assert!(
            harness
                .supervisor
                .world()
                .attached_native_spawn_receipt(started_lsn)
                .is_some()
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .current_for_attachment(attachment_lsn)
                .is_none()
        );
        let mut restarted = replay_prefix(&entries);
        let effects = restarted.step(Input::Started);
        assert!(!effects.iter().filter_map(crate::testing::Seen::record).any(|record|
            matches!(record, Record::InstructionObservationFinished { owner: found, .. } if *found == owner)));
        assert!(
            restarted
                .supervisor
                .world()
                .attached_native_spawn_receipt(started_lsn)
                .is_some()
        );
    }

    #[test]
    fn legacy_issue_recovery_uses_only_the_actual_issue_and_is_replay_idempotent() {
        let (mut entries, agent, turn) =
            crate::testing::journal_with_issued_turn(None, "unchanged task");
        let mut harness = replay_prefix(&entries);
        let effects = harness.step(Input::Started);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::SampleInstructions { .. }))
        );
        let (token, entry) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry } => Some((*token, entry.clone())),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(&entry.record, Record::InstructionObservationFinished {
            baseline: InstructionBaselineRef::MissingIssueBoundary { turn: found, issue_lsn: Lsn(3) },
            settlement: InstructionSettlement::Unknown { reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None, sampled_at: None, recovery: true, comparison }, ..
        } if *found == turn && comparison.changes.is_empty()
            && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::MissingBaseline)
            && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::UnrecordedLaunchBoundary))
        );
        harness.step(Input::Persisted { token, lsn: Lsn(4) });
        let attempt = harness
            .supervisor
            .world()
            .instructions
            .attempt(Lsn(3))
            .unwrap();
        assert!(attempt.snapshot.is_none());
        assert!(attempt.boundary_settled.is_none());
        assert_eq!(
            harness.supervisor.world().turns[&turn].task,
            "unchanged task"
        );
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );

        entries.push(entry);
        let mut replayed = replay_prefix(&entries);
        let effects = replayed.step(Input::Started);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(
                    record,
                    Record::InstructionObservationFinished {
                        baseline: InstructionBaselineRef::MissingIssueBoundary { .. },
                        ..
                    }
                ))
        );
        assert!(
            replayed
                .supervisor
                .world()
                .instructions
                .has_attempt_for_turn(turn)
        );
    }

    #[test]
    fn indexed_recovery_preserves_a_retired_legacy_open_turn_without_prior_observation() {
        let (mut entries, agent, turn) =
            crate::testing::journal_with_issued_turn(None, "retained task");
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::AgentRetired {
                id: agent,
                reason: "stop owned work".into(),
                browser_retire: None,
            },
        });
        let mut harness = replay_prefix(&entries);
        assert!(
            harness
                .supervisor
                .world()
                .active_folder_user_ids()
                .next()
                .is_none()
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .unresolved_owners()
                .next()
                .is_none()
        );
        assert_eq!(
            harness
                .supervisor
                .world()
                .open_turn_owner_ids()
                .copied()
                .collect::<Vec<_>>(),
            vec![agent]
        );
        let effects = harness.step(Input::Started);
        let (token, entry) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry } => Some((*token, entry.clone())),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(&entry.record, Record::InstructionObservationFinished {
            owner: InstructionObservationOwner::Managed { agent: captured_agent, turn: captured_turn, .. },
            baseline: InstructionBaselineRef::MissingIssueBoundary { turn: missing, issue_lsn: Lsn(3) },
            settlement: InstructionSettlement::Unknown { reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None, sampled_at: None, recovery: true, comparison }, ..
        } if *captured_agent == agent && *captured_turn == turn && *missing == turn && comparison.changes.is_empty()
            && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::MissingBaseline))
        );
        let effects = harness.step(Input::Persisted { token, lsn: Lsn(5) });
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            crate::testing::Seen::Spawn { .. } | crate::testing::Seen::SampleInstructions { .. }
        )));
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );
        assert_eq!(
            harness.supervisor.world().turns[&turn].task,
            "retained task"
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_none()
        );

        entries.push(entry);
        let mut replayed = replay_prefix(&entries);
        let effects = replayed.step(Input::Started);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(record, Record::InstructionObservationFinished { .. }))
        );
        assert!(
            replayed
                .supervisor
                .world()
                .instructions
                .unresolved_owners()
                .any(|owner| owner == agent)
        );
    }

    #[test]
    fn active_legacy_ended_ticket_gets_missing_baseline_without_a_synthetic_sample() {
        let (mut entries, _, turn, goal) = ended_legacy_ticket();
        let mut harness = replay_prefix(&entries);
        let effects = harness.step(Input::Started);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::SampleInstructions { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, crate::testing::Seen::Persist { .. }))
        );
        let (request, association) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::ResolveInstructionRecoveryTicket {
                    request,
                    association,
                } => Some((*request, *association)),
                _ => None,
            })
            .unwrap();
        let effects = harness.step(Input::InstructionRecoveryTicketResolved {
            request,
            ticket: Some(association),
        });
        let (token, entry) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry } => Some((*token, entry.clone())),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(&entry.record, Record::InstructionObservationFinished {
            ticket: Some(InstructionTicketAssociation { assignment_lsn: Lsn(2), seq_at_start: dudes_types::Seq(10), .. }),
            baseline: InstructionBaselineRef::MissingTurnBoundary { turn: found, end_lsn: Lsn(5) },
            settlement: InstructionSettlement::Unknown { reason: InstructionUnknownReason::MissingBaseline,
                end: Some(InstructionEndBoundary::ManagedTurn { turn: ended, end_lsn: Lsn(5) }), sampled_at: None,
                recovery: true, comparison }, ..
        } if *found == turn && *ended == turn && comparison.changes.is_empty()
            && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::MissingBaseline))
        );
        let effects = harness.step(Input::Persisted { token, lsn: Lsn(6) });
        assert!(effects.iter().any(|effect| matches!(effect, crate::testing::Seen::RunPredicate { goal: found } if *found == goal)));
        entries.push(entry);
        let mut replayed = replay_prefix(&entries);
        let effects = replayed.step(Input::Started);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(
                    record,
                    Record::InstructionObservationFinished {
                        baseline: InstructionBaselineRef::MissingTurnBoundary { .. },
                        ..
                    }
                ))
        );
        assert!(effects.iter().any(|effect| matches!(effect, crate::testing::Seen::RunPredicate { goal: found } if *found == goal)));
    }

    #[test]
    fn a_closed_ticket_resolution_preserves_uncertainty_without_reopening_its_card() {
        let (entries, agent, turn, goal) = ended_legacy_ticket();
        let mut harness = replay_prefix(&entries);
        let effects = harness.step(Input::Started);
        let request = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::ResolveInstructionRecoveryTicket { request, .. } => {
                    Some(*request)
                }
                _ => None,
            })
            .unwrap();
        assert!(harness.step(Input::Started).is_empty());
        assert!(harness.supervisor.ledger.is_empty());
        let effects = harness.step(Input::InstructionRecoveryTicketResolved {
            request,
            ticket: None,
        });
        let (token, entry) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry } => Some((*token, entry.clone())),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(&entry.record, Record::InstructionObservationFinished {
            ticket: None,
            baseline: InstructionBaselineRef::MissingTurnBoundary { turn: owned, end_lsn: Lsn(5) },
            settlement: InstructionSettlement::Unknown { reason: InstructionUnknownReason::MissingBaseline,
                sampled_at: None, recovery: true, .. }, ..
        } if *owned == turn)
        );
        assert!(harness.supervisor.instructions.recovering());
        assert!(
            harness
                .step(Input::InstructionRecoveryTicketResolved {
                    request,
                    ticket: None
                })
                .is_empty()
        );
        let effects = harness.step(Input::Persisted { token, lsn: Lsn(6) });
        assert!(effects.iter().any(|effect| matches!(effect, crate::testing::Seen::RunPredicate { goal: checked } if *checked == goal)));
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .pending_cards()
                .next()
                .is_none()
        );
        assert!(
            harness
                .step(Input::InstructionReviewAcknowledged { agent, goal })
                .is_empty()
        );
        assert!(harness.supervisor.instructions.recovering());
        let effects = harness.step(Input::PredicateDone {
            goal,
            pass: false,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["npm".into(), "test".into()],
                program: None,
                exit: None,
                output_tail: String::new(),
            },
        });
        let token = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::Persist { token, entry }
                    if matches!(entry.record, Record::PredicateResult { .. }) =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .unwrap();
        assert!(harness.supervisor.instructions.recovering());
        harness.step(Input::Persisted { token, lsn: Lsn(7) });
        assert!(!harness.supervisor.instructions.recovering());
    }

    #[test]
    fn replaced_or_unknown_ticket_responses_cannot_mutate_or_consume_the_original_lookup() {
        let (entries, _, turn, _) = ended_legacy_ticket();
        let mut harness = replay_prefix(&entries);
        let effects = harness.step(Input::Started);
        let (request, association) = effects
            .iter()
            .find_map(|effect| match effect {
                crate::testing::Seen::ResolveInstructionRecoveryTicket {
                    request,
                    association,
                } => Some((*request, *association)),
                _ => None,
            })
            .unwrap();
        let replacement = InstructionTicketAssociation {
            assignment_lsn: Lsn(99),
            ..association
        };
        assert!(
            harness
                .step(Input::InstructionRecoveryTicketResolved {
                    request,
                    ticket: Some(replacement)
                })
                .is_empty()
        );
        assert!(
            harness
                .step(Input::InstructionRecoveryTicketResolved {
                    request: InstructionRequest(request.0 + 99),
                    ticket: Some(association)
                })
                .is_empty()
        );
        assert!(harness.supervisor.ledger.is_empty());
        assert_eq!(harness.supervisor.instructions.recovery_tickets.len(), 1);
        let effects = harness.step(Input::InstructionRecoveryTicketResolved {
            request,
            ticket: None,
        });
        assert!(effects.iter().filter_map(crate::testing::Seen::record).any(|record| matches!(record,
            Record::InstructionObservationFinished { ticket: None,
                baseline: InstructionBaselineRef::MissingTurnBoundary { turn: owned, end_lsn: Lsn(5) }, .. }
                if *owned == turn)));
        assert!(harness.supervisor.instructions.recovery_tickets.is_empty());
        assert!(
            harness
                .step(Input::InstructionRecoveryTicketResolved {
                    request,
                    ticket: Some(association)
                })
                .is_empty()
        );
    }

    #[test]
    fn legacy_recovery_does_not_reopen_a_historical_completed_goal() {
        let (mut entries, agent, turn, goal) = ended_legacy_ticket();
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::PredicateResult {
                goal,
                turn: Some(turn),
                provenance: Provenance::Observed,
                pass: true,
                evidence: dudes_proto::Evidence::Command {
                    argv: vec!["npm".into(), "test".into()],
                    program: Some(
                        if cfg!(windows) {
                            r"C:\fixture\npm.exe"
                        } else {
                            "/fixture/npm"
                        }
                        .into(),
                    ),
                    exit: Some(0),
                    output_tail: String::new(),
                },
            },
        });
        entries.push(Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::LoopDecided {
                agent,
                input_digest: String::new(),
                decision: dudes_proto::LoopDecision::Done,
                gate_trace: Vec::new(),
            },
        });
        let mut harness = replay_prefix(&entries);
        assert!(
            harness
                .supervisor
                .world()
                .completed_goal_lsn(goal)
                .is_some()
        );
        let effects = harness.step(Input::Started);
        assert!(
            !effects
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(
                    record,
                    Record::InstructionObservationStarted { .. }
                        | Record::InstructionObservationFinished { .. }
                ))
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            crate::testing::Seen::SampleInstructions { .. }
                | crate::testing::Seen::RunPredicate { .. }
        )));
    }

    #[test]
    fn runtime_instruction_receipts_fold_only_allowed_increasing_positions_without_deciding() {
        let (mut harness, agent, turn) = refused_attempt();
        let (_, attempt) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::InstructionObservationFinished {
                owner: attempt.owner,
                ticket: attempt.ticket,
                baseline: attempt.baseline,
                settlement: InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::CleanupUnconfirmed,
                    end: None,
                    sampled_at: None,
                    recovery: false,
                    comparison: InstructionComparison {
                        changes: Vec::new(),
                        gaps: vec![gap(InstructionGapKind::CleanupUnconfirmed, agent)],
                        usage: InstructionSampleUsage::default(),
                    },
                },
            },
        };
        let lsn = Lsn(harness.supervisor.world().last_applied_lsn.unwrap().0 + 1);
        assert!(
            harness
                .step(Input::InstructionJournaled {
                    lsn,
                    entry: entry.clone()
                })
                .is_empty()
        );
        let state = harness.supervisor.world().instructions.clone();
        assert_eq!(harness.supervisor.world().last_applied_lsn, Some(lsn));
        assert!(
            harness
                .step(Input::InstructionJournaled {
                    lsn,
                    entry: entry.clone()
                })
                .is_empty()
        );
        assert!(
            harness
                .step(Input::InstructionJournaled {
                    lsn: Lsn::ZERO,
                    entry
                })
                .is_empty()
        );
        let unrelated = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent,
                browser: None,
            },
        };
        assert!(
            harness
                .step(Input::InstructionJournaled {
                    lsn: Lsn(lsn.0 + 1),
                    entry: unrelated
                })
                .is_empty()
        );
        assert!(!harness.supervisor.world().agents[&agent].paused);
        assert_eq!(harness.supervisor.world().instructions, state);
        assert_eq!(harness.supervisor.world().last_applied_lsn, Some(lsn));
    }

    #[test]
    fn recovery_settles_a_durable_prospective_baseline_without_inventing_an_issue() {
        let mut harness = Harness::new();
        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::TurnIssued);
        let (agent, _) = harness.hire_with_goal(&["/usr/bin/false"]);
        let (baseline, owner) = harness
            .supervisor
            .world()
            .instructions
            .unfinished()
            .map(|(_, attempt)| (attempt.baseline, attempt.owner))
            .next()
            .unwrap();
        harness.drive(Input::Command(Command::Pause {
            agent,
            browser: None,
        }));
        harness.restart();
        let finished = harness
            .journal
            .iter()
            .filter(|entry| {
                matches!(&entry.record,
            Record::InstructionObservationFinished { owner: found, baseline: found_baseline,
                settlement: InstructionSettlement::NoIssuedTurn, .. }
                if *found == owner && *found_baseline == baseline)
            })
            .count();
        assert_eq!(finished, 1);
        assert!(
            !harness
                .journal
                .iter()
                .any(|entry| matches!(&entry.record, Record::TurnIssued { .. }))
        );
        harness.restart();
        assert_eq!(
            harness
                .journal
                .iter()
                .filter(|entry| matches!(&entry.record,
            Record::InstructionObservationFinished { baseline: found, .. } if *found == baseline))
                .count(),
            1
        );
        assert!(harness.host.spawned.is_empty());
    }

    #[test]
    fn recovery_retains_issued_task_and_uncertainty_before_retry() {
        let (mut harness, agent, turn) = refused_attempt();
        let task = harness.supervisor.world().turns[&turn].task.clone();
        let (_, attempt) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let baseline = attempt.baseline;
        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::InstructionObservationFinished);
        let trace_start = harness.trace.len();
        harness.restart();
        let recovered = &harness.trace[trace_start..];
        assert!(recovered.iter().filter_map(crate::testing::Seen::record).any(|record| matches!(record,
            Record::InstructionObservationFinished { baseline: found,
                settlement: InstructionSettlement::Unknown { reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    end: None, sampled_at: None, recovery: true, comparison }, .. }
                if *found == baseline && comparison.changes.is_empty()
                    && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::UnrecordedLaunchBoundary))));
        assert!(!recovered.iter().any(|seen| matches!(
            seen,
            crate::testing::Seen::Spawn { .. } | crate::testing::Seen::SampleInstructions { .. }
        )));
        assert_eq!(harness.supervisor.world().turns[&turn].task, task);
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );

        harness
            .unfsynced
            .remove(&dudes_proto::RecordKind::InstructionObservationFinished);
        harness.restart();
        let attempt = match baseline {
            InstructionBaselineRef::Started { lsn } => harness
                .supervisor
                .world()
                .instructions
                .attempt(lsn)
                .unwrap(),
            _ => panic!("the fixture has a durable starting record"),
        };
        assert!(matches!(
            &attempt.finished.as_ref().unwrap().settlement,
            InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                ..
            }
        ));
        assert!(attempt.boundary_settled.is_none());
        assert_eq!(harness.supervisor.world().turns[&turn].task, task);
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .unresolved_launch_attempts(agent)
                .any(|(_, attempt)| attempt.baseline == baseline)
        );
    }

    #[test]
    fn ended_recovery_samples_stored_baseline_and_waits_for_observed_check_fsync() {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["/usr/bin/false"]);
        let turn = harness.open_turn(agent).unwrap();
        let (_, attempt) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let baseline = attempt.baseline;
        let snapshot = attempt.snapshot.clone().unwrap();
        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::InstructionObservationFinished);
        harness.drive(exited(turn));
        let end_lsn = harness
            .supervisor
            .world()
            .instructions
            .end_lsn(turn)
            .unwrap();
        harness
            .unfsynced
            .remove(&dudes_proto::RecordKind::InstructionObservationFinished);
        let trace_start = harness.trace.len();
        harness.restart();
        let recovery = &harness.trace[trace_start..];
        assert!(recovery.iter().any(|seen| matches!(seen,
            crate::testing::Seen::SampleInstructions { baseline: Some(found), .. }
                if **found == snapshot)));
        assert!(recovery.iter().filter_map(crate::testing::Seen::record).any(|record| matches!(record,
            Record::InstructionObservationFinished { baseline: found,
                settlement: InstructionSettlement::Compared { recovery: true,
                    end: InstructionEndBoundary::ManagedTurn { turn: ended, end_lsn: ended_lsn }, .. }, .. }
                if *found == baseline && *ended == turn && *ended_lsn == end_lsn)));
        assert!(recovery.iter().any(|seen| matches!(seen,
            crate::testing::Seen::RunPredicate { goal: found, .. } if *found == goal)));
        assert!(
            !recovery
                .iter()
                .any(|seen| matches!(seen, crate::testing::Seen::Spawn { .. }))
        );

        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::PredicateResult);
        harness.drive(Input::PredicateDone {
            goal,
            pass: false,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["/usr/bin/false".into()],
                program: Some("/usr/bin/false".into()),
                exit: Some(1),
                output_tail: String::new(),
            },
        });
        assert!(harness.supervisor.instructions.recovering());
        assert!(harness.supervisor.pending_instruction_check(agent));
        assert_eq!(harness.supervisor.world().agents[&agent].open_turn, None);
        let trace_start = harness.trace.len();
        harness.restart();
        assert!(
            harness.trace[trace_start..]
                .iter()
                .any(|seen| matches!(seen,
            crate::testing::Seen::RunPredicate { goal: found, .. } if *found == goal))
        );
        assert!(!harness.trace[trace_start..].iter().any(|seen| matches!(
            seen,
            crate::testing::Seen::Spawn { .. } | crate::testing::Seen::SampleInstructions { .. }
        )));
    }

    fn ended_instruction_attempt() -> (Harness, AgentId, TurnId, GoalId) {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["/usr/bin/false"]);
        let turn = harness.open_turn(agent).unwrap();
        harness.drive(exited(turn));
        assert!(harness.supervisor.world().turns[&turn].has_ended());
        assert!(harness.journal.iter().any(|entry| matches!(entry.record,
            Record::InstructionObservationFinished {
                owner: InstructionObservationOwner::Managed { turn: ended, .. },
                settlement: InstructionSettlement::Compared {
                    end: InstructionEndBoundary::ManagedTurn { .. }, ..
                }, ..
            } if ended == turn)));
        (harness, agent, turn, goal)
    }

    fn observed_failure(goal: GoalId) -> Input {
        Input::PredicateDone {
            goal,
            pass: false,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["/usr/bin/false".into()],
                program: Some("/usr/bin/false".into()),
                exit: Some(1),
                output_tail: String::new(),
            },
        }
    }

    fn save_observed_failure_without_decision(harness: &mut Harness, goal: GoalId) {
        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::LoopDecided);
        harness.drive(observed_failure(goal));
        harness
            .unfsynced
            .remove(&dudes_proto::RecordKind::LoopDecided);
    }

    fn recovered_checks(effects: &[crate::testing::Seen], goal: GoalId) -> usize {
        effects
            .iter()
            .filter(|effect| {
                matches!(effect,
            crate::testing::Seen::RunPredicate { goal: checked } if *checked == goal)
            })
            .count()
    }

    fn ended_no_callback_goal(predicate: dudes_proto::Predicate) -> (Harness, AgentId, GoalId) {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.drive(Input::Command(Command::GoalSet {
            agent,
            text: "finish the restored goal".into(),
            predicate,
            budget: dudes_proto::GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 1,
            baseline: None,
            completion_check: None,
            browser_goal: None,
            goal_replacement: None,
        }));
        let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
        let turn = harness.open_turn(agent).unwrap();
        harness.drive(exited(turn));
        assert!(harness.journal.iter().any(|entry| matches!(entry.record,
            Record::InstructionObservationFinished {
                owner: InstructionObservationOwner::Managed { turn: ended, .. },
                settlement: InstructionSettlement::Compared { .. }, ..
            } if ended == turn)));
        assert!(
            !harness
                .journal
                .iter()
                .any(|entry| matches!(entry.record, Record::PredicateResult { .. }))
        );
        (harness, agent, goal)
    }

    fn assert_restored_goal_without_callback_does_not_block_new_work(
        predicate: dudes_proto::Predicate,
    ) {
        let (mut harness, agent, goal) = ended_no_callback_goal(predicate);
        let prior_draws = harness.supervisor.rng.drawn();
        harness.restart();
        assert!(
            !harness.supervisor.instructions.recovering(),
            "startup recovery must finish without a fabricated predicate callback"
        );
        harness.drive(Input::Command(Command::GoalClear { agent, goal }));
        assert!(harness.supervisor.world().goals[&goal].cleared);
        assert_eq!(harness.supervisor.world().agents[&agent].goal, None);
        while harness.supervisor.rng.drawn() < prior_draws {
            let _ = harness.supervisor.rng.ulid();
        }
        let successor = harness.set_goal(agent, &["/usr/bin/false"]);
        assert_ne!(successor, goal);
        let turn = harness
            .open_turn(agent)
            .expect("the new Command goal must be issued");
        assert_eq!(
            harness.supervisor.world().turns[&turn].goal,
            Some(successor)
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .issue_lsn(turn)
                .is_some()
        );
    }

    #[test]
    fn a_missing_board_callback_after_finished_does_not_block_startup_or_a_new_goal() {
        assert_restored_goal_without_callback_does_not_block_new_work(
            dudes_proto::Predicate::BoardState {
                card: dudes_types::CardId::NIL,
                status: dudes_types::CardStatus::Done,
            },
        );
    }

    #[test]
    fn restored_unsupported_predicates_without_callbacks_do_not_block_new_work() {
        for predicate in [
            dudes_proto::Predicate::PathHash {
                path: "unreadable-file".into(),
                blake3: "0".repeat(64),
            },
            dudes_proto::Predicate::AgentVerify {
                prompt: "verify the restored goal".into(),
                budget: Usd::from_dollars(1),
            },
        ] {
            assert_restored_goal_without_callback_does_not_block_new_work(predicate);
        }
    }

    #[test]
    fn startup_does_not_recheck_a_finished_observed_failure_after_halt() {
        let (mut harness, agent, turn, goal) = ended_instruction_attempt();
        save_observed_failure_without_decision(&mut harness, goal);
        harness.drive(Input::Command(Command::Halt {
            agent,
            text: Some("stop owned work".into()),
        }));
        let boundaries: Vec<_> = harness
            .journal
            .iter()
            .filter_map(|entry| match entry.record {
                Record::TurnEnded { turn: ended, .. }
                | Record::InstructionObservationFinished {
                    owner: InstructionObservationOwner::Managed { turn: ended, .. },
                    ..
                }
                | Record::PredicateResult {
                    turn: Some(ended),
                    pass: false,
                    provenance: Provenance::Observed,
                    ..
                } if ended == turn => Some(entry.record.kind()),
                Record::Halt { .. } => Some(entry.record.kind()),
                _ => None,
            })
            .collect();
        assert_eq!(
            boundaries,
            vec![
                dudes_proto::RecordKind::TurnEnded,
                dudes_proto::RecordKind::InstructionObservationFinished,
                dudes_proto::RecordKind::PredicateResult,
                dudes_proto::RecordKind::Halt
            ]
        );
        for _ in 0..2 {
            let trace_start = harness.trace.len();
            let journal_start = harness.journal.len();
            harness.restart();
            assert_eq!(recovered_checks(&harness.trace[trace_start..], goal), 0);
            assert!(
                !harness.journal[journal_start..]
                    .iter()
                    .any(|entry| matches!(entry.record, Record::PredicateResult { .. }))
            );
            assert!(harness.supervisor.world().agents[&agent].halted);
            assert_eq!(
                harness
                    .supervisor
                    .world()
                    .managed_native_spawn_receipts()
                    .count(),
                1
            );
        }
    }

    #[test]
    fn startup_checks_a_finished_interval_without_a_result_once() {
        let (mut harness, _, turn, goal) = ended_instruction_attempt();
        assert!(
            !harness
                .journal
                .iter()
                .any(|entry| matches!(entry.record, Record::PredicateResult { .. }))
        );
        let trace_start = harness.trace.len();
        harness.restart();
        assert_eq!(recovered_checks(&harness.trace[trace_start..], goal), 1);
        save_observed_failure_without_decision(&mut harness, goal);
        assert!(harness.journal.iter().any(|entry| matches!(entry.record,
            Record::PredicateResult { turn: Some(checked), pass: false,
                provenance: Provenance::Observed, .. } if checked == turn)));
        let trace_start = harness.trace.len();
        harness.restart();
        assert_eq!(recovered_checks(&harness.trace[trace_start..], goal), 0);
    }

    #[test]
    fn startup_rechecks_current_end_when_only_an_older_turn_was_judged() {
        let (mut harness, agent, older, goal) = ended_instruction_attempt();
        harness.drive(observed_failure(goal));
        let current = harness.open_turn(agent).unwrap();
        assert_ne!(current, older);
        harness.drive(exited(current));
        let checked: Vec<_> = harness
            .journal
            .iter()
            .filter_map(|entry| match entry.record {
                Record::PredicateResult { turn, .. } => turn,
                _ => None,
            })
            .collect();
        assert_eq!(checked, vec![older]);
        let trace_start = harness.trace.len();
        harness.restart();
        assert_eq!(
            harness.supervisor.world().goals[&goal].last_turn,
            Some(current)
        );
        assert_eq!(recovered_checks(&harness.trace[trace_start..], goal), 1);
        save_observed_failure_without_decision(&mut harness, goal);
        assert!(matches!(harness.journal.last().unwrap().record,
            Record::PredicateResult { turn: Some(checked), .. } if checked == current));
    }

    #[test]
    fn startup_does_not_recheck_a_finished_observed_failure_after_pause() {
        let (mut harness, agent, _, goal) = ended_instruction_attempt();
        save_observed_failure_without_decision(&mut harness, goal);
        harness.drive(Input::Command(Command::Pause {
            agent,
            browser: None,
        }));
        for _ in 0..2 {
            let trace_start = harness.trace.len();
            harness.restart();
            assert_eq!(recovered_checks(&harness.trace[trace_start..], goal), 0);
            assert!(harness.supervisor.world().agents[&agent].paused);
            assert!(!harness.supervisor.world().agents[&agent].halted);
        }
    }

    #[test]
    fn recovery_standing_controls_preserve_the_ending_observation_without_running_its_check() {
        for control in [
            dudes_proto::RecordKind::Halt,
            dudes_proto::RecordKind::Pause,
            dudes_proto::RecordKind::AgentRetired,
        ] {
            let mut live = Harness::new();
            let (agent, goal) = live.hire_with_goal(&["/usr/bin/false"]);
            let turn = live.open_turn(agent).unwrap();
            live.unfsynced
                .insert(dudes_proto::RecordKind::InstructionObservationFinished);
            live.drive(exited(turn));
            let mut harness = replay_prefix(&live.journal);
            harness.journal = live.journal;
            let effects = harness.step(Input::Started);
            let (request, snapshot) = effects
                .iter()
                .find_map(|effect| match effect {
                    crate::testing::Seen::SampleInstructions {
                        request,
                        baseline: Some(snapshot),
                        ..
                    } => Some((*request, snapshot.clone())),
                    _ => None,
                })
                .expect("the durable end must recover its actual unfinished observation");
            let command = match control {
                dudes_proto::RecordKind::Halt => Command::Halt { agent, text: None },
                dudes_proto::RecordKind::Pause => Command::Pause {
                    agent,
                    browser: None,
                },
                dudes_proto::RecordKind::AgentRetired => Command::Retire {
                    agent,
                    reason: "stop owned work".into(),
                    browser_retire: None,
                },
                _ => unreachable!("the fixture uses only standing controls"),
            };
            let effects = harness.step(Input::Command(command));
            acknowledge_actual_persist(&mut harness, &effects);
            let effects = harness.step(Input::InstructionSampled {
                request,
                snapshot,
                ticket: None,
                comparison: Some(Box::new(InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![gap(InstructionGapKind::DeclarationMissing, agent)],
                    usage: InstructionSampleUsage::default(),
                })),
            });
            let effects = acknowledge_actual_persist(&mut harness, &effects);
            assert_eq!(recovered_checks(&effects, goal), 0, "control {control:?}");
            assert!(matches!(harness.journal.last().unwrap().record,
                Record::InstructionObservationFinished {
                    owner: InstructionObservationOwner::Managed { turn: ended, .. },
                    settlement: InstructionSettlement::Compared { recovery: true, .. }, ..
                } if ended == turn));
            assert!(
                harness
                    .supervisor
                    .world()
                    .instructions
                    .current_for_turn(turn)
                    .is_none()
            );
            assert_eq!(
                harness
                    .supervisor
                    .world()
                    .managed_native_spawn_receipts()
                    .count(),
                1
            );
        }
    }

    #[test]
    fn an_actual_cleanup_uncertainty_keeps_the_exact_owed_turn_without_inventing_absence() {
        let (mut harness, agent, turn) = refused_attempt();
        let task = harness.supervisor.world().turns[&turn].task.clone();
        let (_, attempt) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let baseline = attempt.baseline;
        harness.drive(Input::InstructionLaunchCleanupUnconfirmed { turn });
        harness.drive(Input::InstructionLaunchCleanupUnconfirmed { turn });
        let records: Vec<_> = harness
            .journal
            .iter()
            .filter_map(|entry| {
                if let Record::InstructionObservationFinished {
                    baseline: found,
                    settlement,
                    ..
                } = &entry.record
                {
                    (*found == baseline).then_some(settlement)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::CleanupUnconfirmed,
            end: None, sampled_at: None, recovery: false, comparison,
        } if comparison.changes.is_empty() && comparison.gaps.iter().any(|gap| gap.reason == InstructionGapKind::CleanupUnconfirmed)));
        assert_eq!(harness.supervisor.world().turns[&turn].task, task);
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .unresolved_launch_attempts(agent)
                .any(|(_, attempt)| attempt.baseline == baseline
                    && attempt.boundary_settled.is_none())
        );
    }

    #[test]
    fn recovery_does_not_turn_a_recorded_open_child_into_launch_uncertainty() {
        let mut harness = Harness::new();
        let (agent, _) = harness.hire_with_goal(&["/usr/bin/false"]);
        let turn = harness.open_turn(agent).unwrap();
        harness.probe = false;
        let trace_start = harness.trace.len();
        harness.restart();
        assert!(
            harness.trace[trace_start..]
                .iter()
                .any(|seen| matches!(seen,
            crate::testing::Seen::Probe { turn: found } if *found == turn))
        );
        assert!(
            !harness.trace[trace_start..]
                .iter()
                .filter_map(crate::testing::Seen::record)
                .any(|record| matches!(record, Record::InstructionObservationFinished { .. }))
        );
        assert!(
            harness
                .supervisor
                .world()
                .instructions
                .current_for_turn(turn)
                .is_some()
        );
    }

    fn refused_attempt() -> (Harness, AgentId, TurnId) {
        let mut harness = Harness::new();
        harness.launch = false;
        let (agent, _) = harness.hire_with_goal(&["/usr/bin/false"]);
        let turn = harness.supervisor.world().agents[&agent].open_turn.unwrap();
        (harness, agent, turn)
    }

    fn refuse(harness: &mut Harness, agent: AgentId, turn: TurnId) {
        harness.drive(Input::LaunchAdmissionRefused {
            agent,
            turn: Some(turn),
            dispatch_token: None,
            refusal: crate::SharedFolderRefusal::PeerLivenessUnknown {
                agent: AgentId::from_parts(0, 99),
            },
        });
    }

    #[test]
    fn a_typed_pre_host_refusal_settles_only_its_captured_attempt_and_keeps_the_owed_task() {
        let (mut harness, agent, turn) = refused_attempt();
        let issued = harness.supervisor.world().turns[&turn].task.clone();
        let (_, attempt) = harness
            .supervisor
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap();
        let owner = attempt.owner;
        let baseline = attempt.baseline;
        refuse(&mut harness, agent, turn);
        assert!(harness.journal.iter().map(|entry| &entry.record).any(|record| matches!(record,
            Record::InstructionObservationFinished { owner: captured, baseline: captured_baseline, settlement: InstructionSettlement::NoProcess { proof: InstructionNoProcessProof::NoHostCall, .. }, .. }
                if *captured == owner && *captured_baseline == baseline)));
        assert!(!harness.journal.iter().map(|entry| &entry.record).any(
            |record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn)
        ));
        assert_eq!(harness.supervisor.world().turns[&turn].task, issued);
        assert_eq!(
            harness.supervisor.world().agents[&agent].open_turn,
            Some(turn)
        );
        assert!(harness.host.spawned.is_empty());
        assert_eq!(
            harness
                .supervisor
                .world()
                .instructions
                .unresolved_launch_attempts(agent)
                .count(),
            0
        );
    }

    #[test]
    fn a_crash_after_generic_hold_does_not_reconstruct_the_missing_no_host_assertion() {
        let (mut harness, agent, turn) = refused_attempt();
        harness
            .unfsynced
            .insert(dudes_proto::RecordKind::InstructionObservationFinished);
        refuse(&mut harness, agent, turn);
        for _ in 0..2 {
            let emitted = harness.trace.len();
            harness.restart();
            assert!(
                !harness.trace[emitted..]
                    .iter()
                    .filter_map(crate::testing::Seen::record)
                    .any(|record| matches!(
                        record,
                        Record::InstructionObservationFinished {
                            settlement: InstructionSettlement::NoProcess { .. },
                            ..
                        }
                    ))
            );
            assert!(harness.journal.iter().map(|entry| &entry.record).any(
                |record| matches!(record, Record::AgentHeld { agent: held, .. } if *held == agent)
            ));
            assert!(
                !harness
                    .journal
                    .iter()
                    .map(|entry| &entry.record)
                    .any(|record| matches!(
                        record,
                        Record::InstructionObservationFinished {
                            settlement: InstructionSettlement::NoProcess { .. },
                            ..
                        }
                    ))
            );
            assert_eq!(
                harness
                    .supervisor
                    .world()
                    .instructions
                    .unresolved_launch_attempts(agent)
                    .count(),
                1
            );
            assert!(harness.host.spawned.is_empty());
            assert_eq!(
                harness.supervisor.world().agents[&agent].open_turn,
                Some(turn)
            );
        }
    }
}
