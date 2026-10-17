use anyhow::Result;
use dudes_journal::projections::recovery::RecoveryReceipt;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::{
    RecoveryAttribution, RecoveryContext, RecoveryInvalidationEvidence, RecoveryInvalidationReason,
    RecoveryPlan,
};
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryStateError};
use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::recovery::{
    ExpectedRecovery, RecoveryAllowedChoices, RecoveryAttemptView, RecoveryChoice,
    RecoveryInspectResult, RecoveryInspectUnavailable, RecoveryKnownPrefix,
    RecoveryReadUnavailable, RecoveryReceiptInvalidation, RecoveryReceiptResult,
    RecoveryReceiptState, RecoveryReceiptView, RecoverySubmitRefusal, RecoverySubmitResult,
    RecoveryWaitStatus, RecoveryWaitSummary, ReviewedTicketRound,
};
use dudes_rpc::dispatch::OperatorAdmission;
use dudes_supervisor::input::{
    RecoveryAppend, RecoveryCommand, RecoveryPrefix, RecoveryRefusal, RecoveryStage, RecoveryUpdate,
};
use dudes_supervisor::{Clock, ProcessHost, Rng};
use dudes_types::AgentId;

use super::{Ctx, Daemon, PumpOrigin, PumpReport};
use crate::ports::Answered;
use crate::predicate::PredicateRunner;

mod append;
mod scope;
#[cfg(test)]
mod tests;
pub(super) use append::{AppendSubject, IssueClassification};
pub(super) use scope::input_works;

pub(super) struct CommandContext {
    authority: CommandAuthority,
    command: RecoveryCommand,
    update: RecoveryUpdate,
}

enum CommandAuthority {
    Local(OperatorAdmission),
    Browser {
        identity: crate::floor_session::SessionIdentity,
        intent: Box<dudes_proto::rpc::recovery::RecoverySubmitParams>,
    },
}

#[derive(Debug)]
pub(super) struct NoWrite(pub(super) RecoveryRefusal);

impl std::fmt::Display for NoWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "recovery append refused before writing: {:?}", self.0)
    }
}

impl std::error::Error for NoWrite {}

impl CommandContext {
    pub(super) fn work(&self) -> dudes_proto::recovery::RecoveryWork {
        self.command.expected.work
    }
    fn new(admission: OperatorAdmission, command: RecoveryCommand) -> Self {
        let update = RecoveryUpdate {
            request_id: command.request_id,
            prefix: RecoveryPrefix::default(),
            refusal: None,
        };
        Self {
            authority: CommandAuthority::Local(admission),
            command,
            update,
        }
    }

    fn browser(
        identity: crate::floor_session::SessionIdentity,
        intent: &dudes_proto::rpc::recovery::RecoverySubmitParams,
    ) -> Result<Self, RpcError> {
        let command = crate::ports::recovery_command(
            intent,
            RecoveryAttribution::BrowserSession {
                session_digest: identity.digest(),
            },
        )?;
        let update = RecoveryUpdate {
            request_id: command.request_id,
            prefix: RecoveryPrefix::default(),
            refusal: None,
        };
        Ok(Self {
            authority: CommandAuthority::Browser {
                identity,
                intent: Box::new(*intent),
            },
            command,
            update,
        })
    }

    pub(super) fn join(&self, append: &RecoveryAppend, entry: &dudes_proto::Entry) -> Result<()> {
        anyhow::ensure!(
            append.request_id == self.command.request_id
                && append.work == self.command.expected.work
                && append.digest == self.command.digest
                && append.expected == self.command.expected
                && append.choice == self.command.choice
                && append.attribution == self.command.attribution
                && append.prefix == self.update.prefix,
            "recovery append escaped its admitted command"
        );
        let attribution = match &self.authority {
            CommandAuthority::Local(admission) => RecoveryAttribution::LocalOperator {
                pid: admission.pid(),
                uid: admission.uid(),
            },
            CommandAuthority::Browser { identity, intent } => {
                let attribution = RecoveryAttribution::BrowserSession {
                    session_digest: identity.digest(),
                };
                let original =
                    crate::ports::recovery_command(intent, attribution).map_err(|_| {
                        anyhow::anyhow!("original recovery intent could not be encoded")
                    })?;
                anyhow::ensure!(
                    original == self.command,
                    "recovery differs from its original browser intent"
                );
                attribution
            }
        };
        anyhow::ensure!(
            self.command.attribution == attribution,
            "recovery attribution differs from its admitted actor"
        );
        let matched = match (&entry.record, append.stage) {
            (dudes_proto::Record::RecoveryRequired(required), RecoveryStage::Required) => {
                required.binding.work == append.work
                    && required.binding.issue_lsn == self.command.expected.issue_lsn
                    && required.predecessor == self.command.expected.predecessor
            }
            (dudes_proto::Record::Resume { agent, browser }, RecoveryStage::Resume) => {
                *agent == append.work.agent
                    && match (browser, self.command.attribution) {
                        (None, RecoveryAttribution::LocalOperator { .. }) => true,
                        (Some(browser), RecoveryAttribution::BrowserSession { session_digest }) => {
                            browser.request_id == self.command.request_id
                                && browser.request_digest == self.command.digest
                                && browser.control_lsn == self.command.expected.control_lsn
                                && browser.session_digest == session_digest
                        }
                        _ => false,
                    }
            }
            (dudes_proto::Record::RecoveryAuthorized(authorized), RecoveryStage::Authorized) => {
                authorized.request_id == append.request_id
                    && authorized.request_digest == self.command.digest
                    && authorized.binding.work == append.work
                    && authorized.attribution == self.command.attribution
                    && authorized.binding.issue_lsn == self.command.expected.issue_lsn
                    && authorized.expected_control == self.command.expected.control_lsn
                    && authorized.resume_lsn == self.update.prefix.resume
                    && authorized.resulting_control
                        == self
                            .update
                            .prefix
                            .resume
                            .or(self.command.expected.control_lsn)
            }
            _ => false,
        };
        anyhow::ensure!(
            matched,
            "recovery append stage differs from its parked continuation"
        );
        Ok(())
    }

    pub(super) fn permit<'a>(
        &'a self,
        authority: Option<&crate::floor_session::FloorRecoveryAuthority>,
        grants: crate::floor_session::RecoveryGrants,
    ) -> Result<Option<crate::floor_session::MutationPermit<'a>>, NoWrite> {
        match &self.authority {
            CommandAuthority::Local(_) => Ok(None),
            CommandAuthority::Browser { identity, .. } => {
                if !grants.enabled() {
                    return Err(NoWrite(RecoveryRefusal::AuthorityUnavailable));
                }
                let authority = authority.ok_or(NoWrite(RecoveryRefusal::Unavailable))?;
                authority
                    .permit_identity(identity, &self.command.attribution, grants)
                    .map(Some)
                    .map_err(|error| NoWrite(permit_refusal(error)))
            }
        }
    }

    pub(super) fn refused(&self) -> bool {
        self.update.refusal.is_some()
    }

    pub(super) fn observe(&mut self, update: RecoveryUpdate) -> Result<()> {
        anyhow::ensure!(
            update.request_id == self.command.request_id,
            "recovery result escaped its command"
        );
        self.update = update;
        Ok(())
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn recovery_inspect(
        &mut self,
        agent: AgentId,
        _admission: OperatorAdmission,
    ) -> Result<Answered, RpcError> {
        self.recovery_inspect_checked(agent)
    }

    fn recovery_inspect_checked(&mut self, agent: AgentId) -> Result<Answered, RpcError> {
        let unavailable = |reason| {
            Ok(Answered::RecoveryInspected(
                RecoveryInspectResult::Unavailable { reason },
            ))
        };
        if self.sup.is_closing() {
            return unavailable(RecoveryInspectUnavailable::NotRecoverable);
        }
        let Some(row) = self.sup.world().agents.get(&agent) else {
            return unavailable(RecoveryInspectUnavailable::AgentUnavailable);
        };
        if row.retired || row.attached {
            return unavailable(RecoveryInspectUnavailable::AgentUnavailable);
        }
        if row.halted {
            return unavailable(RecoveryInspectUnavailable::Halted);
        }
        if self
            .ctx
            .admit_recovery_prior_attempt(&self.sup, agent, row.open_turn)
            .is_err()
        {
            return unavailable(RecoveryInspectUnavailable::TurnOpen);
        }
        if !self.ctx.recovery_consent_checkout_clear(&self.sup, agent) {
            return unavailable(RecoveryInspectUnavailable::NotRecoverable);
        }
        let view = match self.sup.recovery_opportunity(agent) {
            Ok(view) => view,
            Err(RecoveryRefusal::Claimed) => {
                return unavailable(RecoveryInspectUnavailable::Claimed);
            }
            Err(_) => return unavailable(RecoveryInspectUnavailable::NotRecoverable),
        };
        let through = self.ctx.projections.last_applied_lsn();
        if self.sup.world().last_applied_lsn != Some(through) {
            return unavailable(RecoveryInspectUnavailable::Unavailable);
        }
        let checked = self
            .ctx
            .writer
            .checked_recovery(&mut self.ctx.projections)
            .and_then(|store| store.facts(&view.required.binding));
        if checked.is_err() {
            return unavailable(RecoveryInspectUnavailable::Unavailable);
        }
        let expected = view.expected;
        let Some(admission) = self.condense_admission(agent) else {
            return unavailable(RecoveryInspectUnavailable::Unavailable);
        };
        let waiting = recovery_waits(
            &self.sup,
            agent,
            matches!(admission, dudes_policy::Admission::Refused { .. }),
        );
        Ok(Answered::RecoveryInspected(
            RecoveryInspectResult::Available {
                through,
                expected: Box::new(ExpectedRecovery {
                    work: expected.work,
                    issue_lsn: expected.issue_lsn,
                    source_lsn: expected.source_lsn,
                    episode: expected.episode,
                    expected_predecessor: expected.predecessor,
                    control_lsn: expected.control_lsn,
                    ticket: expected
                        .ticket
                        .map(|(card, round)| ReviewedTicketRound { card, round }),
                }),
                reason: view.required.reason,
                choices: if view.required.reason
                    == dudes_proto::recovery::RecoveryReason::MissingSpawn
                {
                    RecoveryAllowedChoices::RepeatIssuedFresh
                } else if view.recorded_available {
                    RecoveryAllowedChoices::RecordedOrFresh
                } else {
                    RecoveryAllowedChoices::FreshOnly
                },
                prior_effects_unknown: true,
                waiting,
            },
        ))
    }

    pub(super) fn recovery_inspect_browser(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        agent: AgentId,
    ) -> Result<RecoveryInspectResult, crate::ports::FloorRecoveryReadError> {
        use crate::ports::FloorRecoveryReadError;
        let attribution = RecoveryAttribution::BrowserSession {
            session_digest: session.digest(),
        };
        let Some(authority) = self.ctx.recovery_authority.as_ref() else {
            return Err(FloorRecoveryReadError::Unavailable);
        };
        let permit = authority
            .permit_identity(session, &attribution, self.ctx.recovery_grants())
            .map_err(|_| FloorRecoveryReadError::Forbidden)?;
        drop(permit);
        match self.recovery_inspect_checked(agent) {
            Ok(Answered::RecoveryInspected(view)) => Ok(view),
            _ => Err(FloorRecoveryReadError::Unavailable),
        }
    }

    pub(super) fn recovery_receipt_browser(
        &mut self,
        reader: &crate::floor_session::ReceiptSession,
        request: Hex32,
    ) -> Result<RecoveryReceiptResult, crate::ports::FloorRecoveryReadError> {
        use crate::ports::FloorRecoveryReadError;
        let authorizer = self
            .floor_session_authorizer()
            .ok_or(FloorRecoveryReadError::Unavailable)?;
        if !authorizer.validates_receipt(reader) {
            return Err(FloorRecoveryReadError::Forbidden);
        }
        Ok(self.ctx.recovery_read(request))
    }

    pub(super) fn recovery_receipt(
        &mut self,
        request: Hex32,
        _admission: OperatorAdmission,
    ) -> Answered {
        Answered::RecoveryReceipt(self.ctx.recovery_read(request))
    }

    pub(super) fn recovery_submit(
        &mut self,
        command: RecoveryCommand,
        admission: OperatorAdmission,
    ) -> Answered {
        self.recovery_submit_context(CommandContext::new(admission, command))
    }

    pub(super) fn recovery_submit_browser(
        &mut self,
        session: crate::floor_session::SessionIdentity,
        intent: &dudes_proto::rpc::recovery::RecoverySubmitParams,
    ) -> RecoverySubmitResult {
        let request_id = intent.request_id;
        let Ok(context) = CommandContext::browser(session, intent) else {
            return RecoverySubmitResult::Unknown { request_id };
        };
        let Answered::RecoverySubmitted(result) = self.recovery_submit_context(context) else {
            unreachable!("owned recovery result")
        };
        result
    }

    fn recovery_submit_context(&mut self, context: CommandContext) -> Answered {
        let command = context.command.clone();
        let request = command.request_id;
        let prefix = RecoveryKnownPrefix {
            required: None,
            resume: None,
        };
        let existing = self
            .ctx
            .writer
            .checked_recovery(&mut self.ctx.projections)
            .and_then(|store| store.receipt(request, command.digest));
        match existing {
            Ok(Some(receipt)) => {
                return Answered::RecoverySubmitted(match self.ctx.recovery_view(receipt) {
                    Ok(receipt) => RecoverySubmitResult::Accepted {
                        receipt: Box::new(receipt),
                    },
                    Err(_) => RecoverySubmitResult::Unknown {
                        request_id: request,
                    },
                });
            }
            Err(dudes_journal::Error::RecoveryState(RecoveryStateError::RequestConflict)) => {
                return Answered::RecoverySubmitted(RecoverySubmitResult::Refused {
                    reason: RecoverySubmitRefusal::RequestConflict,
                    prefix,
                });
            }
            Err(_) => {
                return Answered::RecoverySubmitted(RecoverySubmitResult::Unknown {
                    request_id: request,
                });
            }
            Ok(None) => {}
        }
        let refused =
            |reason| Answered::RecoverySubmitted(RecoverySubmitResult::Refused { reason, prefix });
        if self.ctx.journal_writes_stopped()
            || self.ctx.closing_failed_after.is_some()
            || self.sup.is_closing()
        {
            return refused(RecoverySubmitRefusal::Unavailable);
        }
        if self
            .ctx
            .admit_recovery_prior_attempt(
                &self.sup,
                command.expected.work.agent,
                Some(command.expected.work.turn),
            )
            .is_err()
        {
            return refused(RecoverySubmitRefusal::NotRecoverable);
        }
        if !self
            .ctx
            .recovery_consent_checkout_clear(&self.sup, command.expected.work.agent)
        {
            return refused(RecoverySubmitRefusal::NotRecoverable);
        }
        let mut report = PumpReport::Recovery(Box::new(context));
        let result = self.pump_started(
            &mut report,
            PumpOrigin::Ordinary,
            &[],
            move |sup, operation| Ok(sup.recover(command, operation)),
        );
        let PumpReport::Recovery(context) = report else {
            unreachable!("owned recovery report")
        };
        if result.is_err() {
            return Answered::RecoverySubmitted(RecoverySubmitResult::Unknown {
                request_id: request,
            });
        }
        if let Some(reason) = context.update.refusal {
            return Answered::RecoverySubmitted(RecoverySubmitResult::Refused {
                reason: refusal(reason),
                prefix: RecoveryKnownPrefix {
                    required: context.update.prefix.required,
                    resume: context.update.prefix.resume,
                },
            });
        }
        Answered::RecoverySubmitted(match self.ctx.recovery_read(request) {
            RecoveryReceiptResult::Found { receipt } => RecoverySubmitResult::Accepted { receipt },
            _ => RecoverySubmitResult::Unknown {
                request_id: request,
            },
        })
    }
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn recovery_read(&mut self, request: Hex32) -> RecoveryReceiptResult {
        let result = self
            .writer
            .checked_recovery(&mut self.projections)
            .and_then(|store| store.receipt_by_request(request));
        match result {
            Ok(Some(receipt)) => self.recovery_view(receipt).map_or_else(
                |_| RecoveryReceiptResult::Unavailable {
                    reason: RecoveryReadUnavailable::InvalidHistory,
                },
                |receipt| RecoveryReceiptResult::Found {
                    receipt: Box::new(receipt),
                },
            ),
            Ok(None) => RecoveryReceiptResult::Absent {
                checked_through: self.projections.last_applied_lsn(),
            },
            Err(error) => RecoveryReceiptResult::Unavailable {
                reason: read_error(&error),
            },
        }
    }

    fn recovery_view(
        &self,
        receipt: RecoveryReceipt,
    ) -> dudes_journal::Result<RecoveryReceiptView> {
        let authorization = receipt.authorization;
        let choice = match &authorization.plan {
            RecoveryPlan::New {
                context: RecoveryContext::TryRecorded { .. },
                ..
            } => RecoveryChoice::TryRecorded,
            RecoveryPlan::New { .. } => RecoveryChoice::FreshWithRecap,
            RecoveryPlan::RepeatIssued { .. } => RecoveryChoice::RepeatIssuedFresh,
        };
        let turn = match &authorization.plan {
            RecoveryPlan::New { turn, .. } => *turn,
            RecoveryPlan::RepeatIssued { issue, .. } => issue.issued.work.turn,
        };
        let expected_issue = match &receipt.state {
            RecoveryClaimState::IssueBound(issue)
            | RecoveryClaimState::Consumed { issue, .. }
            | RecoveryClaimState::Superseded { issue, .. } => Some((issue.lsn, issue.issued.work)),
            RecoveryClaimState::Pending | RecoveryClaimState::Invalidated { .. } => None,
        };
        let status = match receipt.state {
            RecoveryClaimState::Pending => RecoveryReceiptState::Pending,
            RecoveryClaimState::IssueBound(issue) => RecoveryReceiptState::IssueBound {
                turn: issue.issued.work.turn,
                issue_lsn: issue.lsn,
            },
            RecoveryClaimState::Consumed {
                issue,
                evidence_lsn,
            } => RecoveryReceiptState::Consumed {
                turn: issue.issued.work.turn,
                issue_lsn: issue.lsn,
                evidence_lsn,
            },
            RecoveryClaimState::Invalidated {
                invalidation_lsn,
                evidence,
            } => RecoveryReceiptState::Invalidated {
                invalidation_lsn,
                reason: invalidation(evidence),
            },
            RecoveryClaimState::Superseded {
                issue,
                successor,
                barrier_lsn,
            } => RecoveryReceiptState::Superseded {
                turn: issue.issued.work.turn,
                issue_lsn: issue.lsn,
                successor,
                barrier_lsn,
            },
        };
        let row = self.projections.turn(turn)?;
        match (expected_issue, &row) {
            (Some((issue, work)), Some(row))
                if row.lsn_issued == issue
                    && row.agent == work.agent
                    && row.goal == Some(work.goal)
                    && row.mode == dudes_types::Mode::Managed => {}
            (None, None) => {}
            _ => return Err(RecoveryStateError::Binding.into()),
        }
        Ok(RecoveryReceiptView {
            request_id: authorization.request_id,
            claim: receipt.claim,
            choice,
            status,
            attempt: RecoveryAttemptView {
                spawn: row.as_ref().and_then(|row| row.spawn_lsn),
                confirmation: row.as_ref().and_then(|row| row.confirmation_lsn),
                end: row.as_ref().and_then(|row| row.end_lsn),
            },
            prior_effects_unknown: true,
        })
    }
}

fn read_error(error: &dudes_journal::Error) -> RecoveryReadUnavailable {
    use dudes_journal::error::RecoveryStoreError as E;
    match error {
        dudes_journal::Error::RecoveryStore(E::JournalUncertain)
        | dudes_journal::Error::Poisoned { .. } => RecoveryReadUnavailable::JournalUncertain,
        dudes_journal::Error::RecoveryStore(E::Uncertified) => RecoveryReadUnavailable::Uncertified,
        dudes_journal::Error::RecoveryStore(E::JournalAdvanced) => {
            RecoveryReadUnavailable::StaleCut
        }
        dudes_journal::Error::RecoveryStore(_)
        | dudes_journal::Error::RecoveryState(_)
        | dudes_journal::Error::RecoveryCache(_) => RecoveryReadUnavailable::InvalidHistory,
        _ => RecoveryReadUnavailable::StorageUnavailable,
    }
}

fn invalidation(evidence: RecoveryInvalidationEvidence) -> RecoveryReceiptInvalidation {
    match evidence {
        RecoveryInvalidationEvidence::BrowserAuthorityUnavailable { .. } => {
            RecoveryReceiptInvalidation::AuthorityUnavailable
        }
        RecoveryInvalidationEvidence::RecordedChange { reason, .. } => match reason {
            RecoveryInvalidationReason::GoalChanged => RecoveryReceiptInvalidation::GoalChanged,
            RecoveryInvalidationReason::GoalCompleted => RecoveryReceiptInvalidation::GoalCompleted,
            RecoveryInvalidationReason::TicketChanged => RecoveryReceiptInvalidation::TicketChanged,
            RecoveryInvalidationReason::LeaseChanged => RecoveryReceiptInvalidation::LeaseChanged,
            RecoveryInvalidationReason::ControlChanged => {
                RecoveryReceiptInvalidation::ControlChanged
            }
            RecoveryInvalidationReason::AgentUnavailable => {
                RecoveryReceiptInvalidation::AgentUnavailable
            }
        },
    }
}

pub(super) const fn refusal(reason: RecoveryRefusal) -> RecoverySubmitRefusal {
    match reason {
        RecoveryRefusal::RequestConflict => RecoverySubmitRefusal::RequestConflict,
        RecoveryRefusal::StaleWork => RecoverySubmitRefusal::StaleWork,
        RecoveryRefusal::StaleSource => RecoverySubmitRefusal::StaleSource,
        RecoveryRefusal::StaleEpisode => RecoverySubmitRefusal::StaleEpisode,
        RecoveryRefusal::StalePredecessor => RecoverySubmitRefusal::StalePredecessor,
        RecoveryRefusal::StaleControl => RecoverySubmitRefusal::StaleControl,
        RecoveryRefusal::StaleTicket => RecoverySubmitRefusal::StaleTicket,
        RecoveryRefusal::Claimed => RecoverySubmitRefusal::Claimed,
        RecoveryRefusal::ChoiceUnavailable => RecoverySubmitRefusal::ChoiceUnavailable,
        RecoveryRefusal::NotRecoverable => RecoverySubmitRefusal::NotRecoverable,
        RecoveryRefusal::AuthorityUnavailable => RecoverySubmitRefusal::AuthorityUnavailable,
        RecoveryRefusal::Unavailable => RecoverySubmitRefusal::Unavailable,
    }
}

fn recovery_waits<C: Clock, R: Rng>(
    sup: &dudes_supervisor::Supervisor<C, R>,
    agent: AgentId,
    budget_waiting: bool,
) -> RecoveryWaitSummary {
    let ready = |waiting| {
        if waiting {
            RecoveryWaitStatus::Waiting
        } else {
            RecoveryWaitStatus::Ready
        }
    };
    let row = &sup.world().agents[&agent];
    let goal = row.goal.and_then(|id| sup.world().goals.get(&id));
    let (instruction_review, instruction_comparison) = instruction_waits(
        &sup.world().instructions,
        row.goal
            .and_then(|goal| sup.world().ticket_goal(goal))
            .map(|(card, _)| card),
    );
    RecoveryWaitSummary {
        open_question: ready(row.open_question.is_some()),
        budget: ready(budget_waiting),
        hops: ready(goal.is_some_and(|goal| goal.turns_on_goal >= goal.goal.hop_cap)),
        instruction_review,
        instruction_comparison,
    }
}

pub(super) fn known_no_write(error: &anyhow::Error) -> Option<RecoveryRefusal> {
    if let Some(no_write) = error.downcast_ref::<NoWrite>() {
        return Some(no_write.0);
    }
    if error.downcast_ref::<super::KnownNoEffect>().is_some() {
        return Some(RecoveryRefusal::Unavailable);
    }
    match error.downcast_ref::<dudes_journal::Error>() {
        Some(dudes_journal::Error::RecoveryAdmission { reason, .. }) => Some(match reason {
            RecoveryStateError::Claimed | RecoveryStateError::Lifecycle => RecoveryRefusal::Claimed,
            RecoveryStateError::RequestConflict | RecoveryStateError::DuplicateRequest => {
                RecoveryRefusal::RequestConflict
            }
            RecoveryStateError::Context => RecoveryRefusal::ChoiceUnavailable,
            RecoveryStateError::Control => RecoveryRefusal::StaleControl,
            _ => RecoveryRefusal::StaleWork,
        }),
        _ => None,
    }
}

pub(super) const fn permit_refusal(
    error: crate::floor_session::RecoveryPermitFailure,
) -> RecoveryRefusal {
    match error {
        crate::floor_session::RecoveryPermitFailure::AuthorityUnavailable => {
            RecoveryRefusal::AuthorityUnavailable
        }
        crate::floor_session::RecoveryPermitFailure::Indeterminate => RecoveryRefusal::Unavailable,
    }
}

fn instruction_waits(
    instructions: &dudes_proto::instruction_state::InstructionState,
    card: Option<dudes_types::CardId>,
) -> (RecoveryWaitStatus, RecoveryWaitStatus) {
    let waiting = |value| {
        if value {
            RecoveryWaitStatus::Waiting
        } else {
            RecoveryWaitStatus::Ready
        }
    };
    (
        waiting(card.is_some_and(|card| instructions.ticket_reason(card).is_some())),
        waiting(card.is_some_and(|card| instructions.observation_pending(card))),
    )
}
