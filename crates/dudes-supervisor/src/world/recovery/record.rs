//! Prepared recovery changes retain the actual before-record facts through the ordinary fold.

use dudes_proto::recovery::{RecoveryClaimRef, RecoveryInvalidated, RecoveryIssue, RecoveryWork};
use dudes_proto::recovery_state::history::{RecoveryEvidence, RecoveryHistory, RecoveryWitness};
use dudes_proto::recovery_state::transition::RecoveryTransition;
use dudes_proto::recovery_state::{
    RecoveryAttemptFacts, RecoveryClaimState, RecoveryFacts, RecoveryState, RecoveryStateError,
};
use dudes_proto::{Entry, Record};
use dudes_types::{Lsn, Mode};

use crate::World;

pub(in crate::world) enum PreparedRecovery<'a> {
    Current {
        transition: RecoveryTransition<'a>,
        facts: Box<RecoveryFacts>,
    },
    Invalidated {
        record: &'a RecoveryInvalidated,
        through: Lsn,
    },
    Issued {
        claim: RecoveryClaimRef,
        issue: RecoveryIssue,
        facts: Box<RecoveryFacts>,
    },
    Consumed {
        claim: RecoveryClaimRef,
        issue: RecoveryIssue,
        evidence: Box<RecoveryWitness>,
        facts: Box<RecoveryAttemptFacts>,
    },
}

impl PreparedRecovery<'_> {
    pub(in crate::world) fn apply(
        self,
        lsn: Lsn,
        state: &mut RecoveryState,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        match self {
            Self::Current { transition, facts } => {
                state.apply_transition(lsn, transition, facts.as_ref(), history)
            }
            Self::Invalidated { record, through } => {
                state.apply_invalidation(lsn, record, through, history)
            }
            Self::Issued {
                claim,
                issue,
                facts,
            } => state.apply_transition(
                lsn,
                RecoveryTransition::Issued {
                    claim,
                    issue: &issue,
                },
                facts.as_ref(),
                history,
            ),
            Self::Consumed {
                claim,
                issue,
                evidence,
                facts,
            } => state.apply_consumed(lsn, claim, &issue, evidence.as_ref(), facts.as_ref()),
        }
    }
}

impl World {
    pub(in crate::world) fn prepare_recovery_record<'a>(
        &self,
        lsn: Lsn,
        entry: &'a Entry,
    ) -> Result<Option<PreparedRecovery<'a>>, RecoveryStateError> {
        dudes_proto::recovery::validate_record_shape(&entry.record)?;
        let direct = match &entry.record {
            Record::RecoveryRequired(record) => Some(RecoveryTransition::Required(record)),
            Record::RecoveryAuthorized(record) => Some(RecoveryTransition::Authorized(record)),
            Record::RecoveryInvalidated(record) => {
                self.prepare_recovery(RecoveryTransition::Invalidated(record))?;
                return Ok(Some(PreparedRecovery::Invalidated {
                    record,
                    through: self.last_applied_lsn.ok_or(RecoveryStateError::Position)?,
                }));
            }
            _ => None,
        };
        if let Some(transition) = direct {
            let binding = transition.binding(&self.recovery_state)?;
            let facts = self.recovery_facts(&binding)?;
            self.recovery_state
                .validate_transition(transition, &facts, &self.recovery_history)?;
            self.validate_recovery_new_turn(transition)?;
            return Ok(Some(PreparedRecovery::Current {
                transition,
                facts: Box::new(facts),
            }));
        }
        match &entry.record {
            Record::TurnIssued { turn, .. }
            | Record::TurnIssuedWithBrowserNote { turn, .. }
            | Record::ConversationTurnIssued { turn, .. }
            | Record::NoteOnlyTurnIssued { turn, .. }
            | Record::TaughtNoteOnlyTurnIssued { turn, .. }
            | Record::MailWakeOnlyTurnIssued(dudes_proto::MailWakeIssue { turn, .. }) => {
                let Some(claim) = self.recovery_state.planned_claim(*turn) else {
                    return Ok(None);
                };
                if !matches!(
                    &entry.record,
                    Record::TurnIssued {
                        mode: Mode::Managed,
                        ..
                    } | Record::TurnIssuedWithBrowserNote {
                        mode: Mode::Managed,
                        ..
                    }
                ) {
                    return Err(RecoveryStateError::Binding);
                }
                let (issue, facts) = self.recovery_issued_facts(lsn, entry, claim)?;
                Ok(Some(PreparedRecovery::Issued {
                    claim,
                    issue,
                    facts: Box::new(facts),
                }))
            }
            Record::TurnSpawned { turn, .. } | Record::TurnEnded { turn, .. } => {
                let Some(row) = self.turns.get(turn) else {
                    return Ok(None);
                };
                let Some(goal) = row.goal else {
                    return Ok(None);
                };
                let work = RecoveryWork {
                    agent: row.agent,
                    goal,
                    turn: *turn,
                };
                let Some(claim) = self.recovery_state.work_claim(work)? else {
                    return Ok(None);
                };
                let receipt = self.recovery_state.claim_receipt(claim)?;
                let RecoveryClaimState::IssueBound(issue) = receipt.state else {
                    return Ok(None);
                };
                let source = self
                    .recovery_history
                    .at(row.issue_lsn)
                    .map_err(|_| RecoveryStateError::Binding)?;
                if !matches!(
                    source.evidence(),
                    RecoveryEvidence::Issue {
                        mode: Mode::Managed,
                        ..
                    }
                ) {
                    return Err(RecoveryStateError::Binding);
                }
                let facts = self.recovery_attempt_facts(issue)?;
                let evidence = self.recovery_entry_source(lsn, entry)?;
                self.recovery_state
                    .validate_consumed(claim, issue, &evidence, &facts)?;
                Ok(Some(PreparedRecovery::Consumed {
                    claim,
                    issue: issue.clone(),
                    evidence: Box::new(evidence),
                    facts: Box::new(facts),
                }))
            }
            _ => Ok(None),
        }
    }

    fn recovery_issued_facts(
        &self,
        lsn: Lsn,
        entry: &Entry,
        claim: RecoveryClaimRef,
    ) -> Result<(RecoveryIssue, RecoveryFacts), RecoveryStateError> {
        let evidence = self.recovery_entry_source(lsn, entry)?;
        let issue = evidence.issue().ok_or(RecoveryStateError::Binding)?;
        let transition = RecoveryTransition::Issued {
            claim,
            issue: &issue,
        };
        let binding = transition.binding(&self.recovery_state)?;
        let facts = self.recovery_facts(&binding)?;
        self.recovery_state
            .validate_transition(transition, &facts, &self.recovery_history)?;
        Ok((issue, facts))
    }

    fn recovery_entry_source(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<RecoveryWitness, RecoveryStateError> {
        let mut source = RecoveryHistory::default();
        source
            .observe_with_context(lsn, entry, self.recovery_before(entry), super::hash)
            .map_err(|_| RecoveryStateError::Binding)?;
        source
            .at(lsn)
            .cloned()
            .map_err(|_| RecoveryStateError::Binding)
    }
}
