use super::{
    Clock, Entry, PersistToken, Record, RecoveryClaimState, RecoveryContext, RecoveryPlan,
    RecoveryRefusal, RecoverySubjectKey, RecoverySubjectUse, RecoveryWork, Rng, Supervisor, Then,
};
use crate::input::RecoveryIssueAppend;
use dudes_types::{Mode, TurnId};

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Classify the exact parked issue without supplying permission to append it.
    ///
    /// # Errors
    ///
    /// Refuses a changed parked entry, claim, source binding or prepared launch.
    pub fn recovery_issue_append(
        &self,
        token: PersistToken,
        entry: &Entry,
    ) -> Result<Option<RecoveryIssueAppend>, RecoveryRefusal> {
        let Some(turn) = issued_turn(&entry.record) else {
            return Ok(None);
        };
        let parked = self
            .ledger
            .get(&token)
            .ok_or(RecoveryRefusal::Unavailable)?;
        if &parked.entry != entry {
            return Err(RecoveryRefusal::Unavailable);
        }
        let (state, _) = self
            .world
            .recovery_projection()
            .map_err(|_| RecoveryRefusal::Unavailable)?;
        let Some(claim) = state.planned_claim(turn) else {
            self.require_closed_source_claim(state, &parked.then)?;
            return Ok(None);
        };
        let receipt = state
            .claim_receipt(claim)
            .map_err(|_| RecoveryRefusal::Unavailable)?;
        let Then::Spawn {
            agent,
            turn: prepared,
            launch,
        } = &parked.then
        else {
            return Err(RecoveryRefusal::Unavailable);
        };
        let (Record::TurnIssued {
            agent: owner,
            goal,
            task,
            session,
            model_selection: Some(model_selection),
            mode: Mode::Managed,
            ..
        }
        | Record::TurnIssuedWithBrowserNote {
            agent: owner,
            goal,
            task,
            session,
            model_selection,
            mode: Mode::Managed,
            ..
        }) = &entry.record
        else {
            return Err(RecoveryRefusal::Unavailable);
        };
        let RecoveryPlan::New {
            turn: reserved,
            context,
        } = &receipt.authorization.plan
        else {
            return Err(RecoveryRefusal::Claimed);
        };
        let work = RecoveryWork {
            agent: *agent,
            goal: *goal,
            turn,
        };
        if state
            .work_claim(work)
            .map_err(|_| RecoveryRefusal::Claimed)?
            != Some(claim)
            || !matches!(receipt.state, RecoveryClaimState::Pending)
            || turn != *prepared
            || turn != *reserved
            || owner != agent
            || receipt.authorization.binding.work.agent != *agent
            || receipt.authorization.binding.work.goal != *goal
            || launch.task != *task
            || launch.session != context_session(context)
            || launch.session.id() != session.as_ref()
            || *model_selection != launch.selection
            || receipt.authorization.binding.model_selection.as_ref() != Some(&launch.selection)
        {
            return Err(RecoveryRefusal::StaleWork);
        }
        let facts = self
            .world
            .recovery_facts(&receipt.authorization.binding)
            .map_err(|_| RecoveryRefusal::StaleWork)?;
        if facts.control_lsn != receipt.authorization.resulting_control {
            return Err(RecoveryRefusal::StaleControl);
        }
        Ok(Some(RecoveryIssueAppend {
            subject: RecoverySubjectKey::new(
                claim,
                receipt.authorization.clone(),
                RecoverySubjectUse::AdmitNew,
            ),
            turn,
        }))
    }

    fn require_closed_source_claim(
        &self,
        state: &dudes_proto::recovery_state::RecoveryState,
        then: &Then,
    ) -> Result<(), RecoveryRefusal> {
        if let Then::Spawn { agent, .. } = then
            && let Ok(binding) = self.recovery_binding(*agent)
            && let Some(claim) = state
                .work_claim(binding.work)
                .map_err(|_| RecoveryRefusal::Claimed)?
        {
            let receipt = state
                .claim_receipt(claim)
                .map_err(|_| RecoveryRefusal::Unavailable)?;
            if !matches!(receipt.state, RecoveryClaimState::Consumed { .. }) {
                return Err(RecoveryRefusal::Claimed);
            }
        }
        Ok(())
    }

    /// Retire only this exact unwritten launch after the executor proves no frame was written.
    #[must_use]
    pub fn recovery_issue_not_written(
        &mut self,
        token: PersistToken,
        entry: &Entry,
        expected: &RecoveryIssueAppend,
    ) -> bool {
        if self
            .recovery_issue_append(token, entry)
            .ok()
            .flatten()
            .as_ref()
            != Some(expected)
        {
            return false;
        }
        self.ledger.remove(&token);
        true
    }
}

fn issued_turn(record: &Record) -> Option<TurnId> {
    match record {
        Record::TurnIssued { turn, .. }
        | Record::TurnIssuedWithBrowserNote { turn, .. }
        | Record::ConversationTurnIssued { turn, .. }
        | Record::NoteOnlyTurnIssued { turn, .. }
        | Record::TaughtNoteOnlyTurnIssued { turn, .. }
        | Record::MailWakeOnlyTurnIssued(dudes_proto::MailWakeIssue { turn, .. }) => Some(*turn),
        _ => None,
    }
}

fn context_session(context: &RecoveryContext) -> dudes_providers::adapter::TurnSession {
    match context {
        RecoveryContext::TryRecorded { session } => {
            dudes_providers::adapter::TurnSession::Resume(session.clone())
        }
        RecoveryContext::Fresh { session, .. } => session.clone().map_or(
            dudes_providers::adapter::TurnSession::Assigned,
            dudes_providers::adapter::TurnSession::First,
        ),
    }
}
