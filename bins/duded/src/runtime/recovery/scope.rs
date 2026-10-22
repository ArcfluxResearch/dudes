use super::super::{Ctx, Shared};
use crate::floor_session::RecoveryGrants;
use crate::predicate::PredicateRunner;
use dudes_journal::projections::recovery::{CheckedClaimSubject, CheckedWorkSubject};
use dudes_proto::Record;
use dudes_proto::recovery::{RecoveryPlan, RecoveryWork};
use dudes_proto::recovery_state::RecoveryClaimState;
use dudes_supervisor::{
    Clock, Input, ProcessHost, RecoveryAvailability, RecoveryOperation, RecoverySubjectKey,
    RecoverySubjectUse, Rng, Supervisor, World, with_recovery_operation,
};

pub(in crate::runtime) fn input_works(input: &Input) -> Vec<RecoveryWork> {
    let entry = match input {
        Input::Persisted { append, .. }
        | Input::Journaled { append }
        | Input::InstructionJournaled { append }
        | Input::BoardChanged { append, .. } => append.entry(),
        Input::Replayed { entry, .. } => entry,
        _ => return Vec::new(),
    };
    record_works(&entry.record)
}

fn record_works(record: &Record) -> Vec<RecoveryWork> {
    match record {
        Record::RecoveryRequired(required) => vec![required.binding.work],
        Record::RecoveryAuthorized(authorization) => {
            let target = match &authorization.plan {
                RecoveryPlan::New { turn, .. } => RecoveryWork {
                    turn: *turn,
                    ..authorization.binding.work
                },
                RecoveryPlan::RepeatIssued { issue, .. } => issue.issued.work,
            };
            vec![authorization.binding.work, target]
        }
        Record::TurnIssued {
            agent, goal, turn, ..
        }
        | Record::TurnIssuedWithBrowserNote {
            agent, goal, turn, ..
        } => {
            vec![RecoveryWork {
                agent: *agent,
                goal: *goal,
                turn: *turn,
            }]
        }
        _ => Vec::new(),
    }
}

fn key(subject: &CheckedClaimSubject) -> Option<RecoverySubjectKey> {
    let receipt = subject.receipt();
    if !matches!(
        receipt.authorization.attribution,
        dudes_proto::recovery::RecoveryAttribution::BrowserSession { .. }
    ) {
        return None;
    }
    let purpose = match &receipt.state {
        RecoveryClaimState::Pending => RecoverySubjectUse::AdmitNew,
        RecoveryClaimState::IssueBound(issue) => RecoverySubjectUse::DispatchIssued(issue.clone()),
        _ => return None,
    };
    subject.current_facts()?;
    Some(RecoverySubjectKey::new(
        receipt.claim,
        receipt.authorization.clone(),
        purpose,
    ))
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(in crate::runtime) fn recovery_grants(&self) -> RecoveryGrants {
        let live = self.switches.load();
        RecoveryGrants {
            recovery: live.serves_recovery(),
            pause_resume: live.control.pause_resume(),
        }
    }

    pub(in crate::runtime) fn detached_recovery_subjects(
        &mut self,
        world: &World,
        extra: &[RecoveryWork],
    ) -> dudes_journal::Result<Vec<CheckedWorkSubject>> {
        let mut works: std::collections::BTreeSet<_> = world
            .turns
            .iter()
            .filter_map(|(turn, row)| {
                row.goal.map(|goal| RecoveryWork {
                    agent: row.agent,
                    goal,
                    turn: *turn,
                })
            })
            .collect();
        works.extend(extra.iter().copied());
        self.writer
            .checked_recovery(&mut self.projections)?
            .authority_subjects(&works.into_iter().collect::<Vec<_>>())
    }

    pub(in crate::runtime) fn with_recovery_facts<R: Rng, T>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        extra: &[RecoveryWork],
        use_once: impl for<'op> FnOnce(&mut Supervisor<Shared<C>, R>, &RecoveryOperation<'op>) -> T,
    ) -> T {
        let subjects = self.detached_recovery_subjects(sup.world(), extra);
        let mut keys = Vec::new();
        if let Ok(subjects) = &subjects {
            for subject in subjects {
                if let CheckedWorkSubject::Claim(subject) = subject
                    && let Some(key) = key(subject)
                    && !keys.contains(&key)
                {
                    keys.push(key);
                }
            }
        }
        with_recovery_operation(
            &keys,
            |key| self.observe_recovery(key),
            |operation| use_once(sup, operation),
        )
    }

    pub(in crate::runtime) fn handle_recovery_input<R: Rng>(
        &mut self,
        sup: &mut Supervisor<Shared<C>, R>,
        input: Input,
        command_work: Option<RecoveryWork>,
    ) -> anyhow::Result<dudes_supervisor::input::Handled> {
        let input = self.current_mail_response(input, sup);
        #[cfg(test)]
        if let Some(before) = self.before_recovery_input.as_mut() {
            before(&input);
        }
        let mut works = input_works(&input);
        works.extend(command_work);
        self.with_recovery_facts(sup, &works, |sup, operation| {
            sup.handle(input, operation).map_err(anyhow::Error::new)
        })
    }

    fn observe_recovery(&self, key: &RecoverySubjectKey) -> RecoveryAvailability {
        let grants = self.recovery_grants();
        if grants.enabled() {
            self.recovery_authority
                .as_ref()
                .map_or(RecoveryAvailability::Indeterminate, |authority| {
                    authority.observe_original(&key.authorization().attribution, grants)
                })
        } else {
            RecoveryAvailability::EstablishedUnavailable
        }
    }

    pub(in crate::runtime) fn recovery_dispatch_allowed<R: Rng>(
        &mut self,
        sup: &Supervisor<Shared<C>, R>,
        turn: dudes_types::TurnId,
    ) -> bool {
        if !self.recovery_turn_checkout_clear(sup, turn) {
            return false;
        }
        let Some(row) = sup.world().turns.get(&turn) else {
            return false;
        };
        let Some(goal) = row.goal else {
            return sup.recovery_launch_allowed(row.agent, turn, &RecoveryOperation::empty());
        };
        let work = RecoveryWork {
            agent: row.agent,
            goal,
            turn,
        };
        let checked = self
            .writer
            .checked_recovery(&mut self.projections)
            .and_then(|store| store.authority_subjects(&[work]));
        let Ok(mut subjects) = checked else {
            return false;
        };
        let Some(subject) = subjects.pop() else {
            return false;
        };
        let Ok((state, _)) = sup.world().recovery_projection() else {
            return false;
        };
        let canonical = match &subject {
            CheckedWorkSubject::NoClaim { .. } => {
                state.work_claim(work).is_ok_and(|claim| claim.is_none())
            }
            CheckedWorkSubject::UnclaimedBoundary { .. } => false,
            CheckedWorkSubject::Claim(subject) => {
                let receipt = subject.receipt();
                state
                    .work_claim(work)
                    .is_ok_and(|claim| claim == Some(receipt.claim))
                    && state.claim_receipt(receipt.claim).is_ok_and(|current| {
                        current.authorization == &receipt.authorization
                            && current.state == &receipt.state
                    })
            }
        };
        if !canonical {
            return false;
        }
        let keys = match &subject {
            CheckedWorkSubject::Claim(subject) => key(subject).into_iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        with_recovery_operation(
            &keys,
            |key| self.observe_recovery(key),
            |operation| sup.recovery_launch_allowed(work.agent, work.turn, operation),
        )
    }
}
