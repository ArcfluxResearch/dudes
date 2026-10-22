use std::marker::PhantomData;

use dudes_proto::recovery::{RecoveryAuthorized, RecoveryClaimRef, RecoveryIssue};

/// The exact admission or durable dispatch named by a recovery subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverySubjectUse {
    /// Admit the authorization's still-pending new turn.
    AdmitNew,
    /// Dispatch this exact already-issued task and position.
    DispatchIssued(RecoveryIssue),
}

/// Historical identity to join again to the machine before using a live observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverySubjectKey {
    claim: RecoveryClaimRef,
    authorization: RecoveryAuthorized,
    purpose: RecoverySubjectUse,
}

impl RecoverySubjectKey {
    /// Retain factual identity only. Construction grants no admission or write permission.
    #[must_use]
    pub const fn new(
        claim: RecoveryClaimRef,
        authorization: RecoveryAuthorized,
        purpose: RecoverySubjectUse,
    ) -> Self {
        Self {
            claim,
            authorization,
            purpose,
        }
    }

    /// Exact episode and authorization position.
    #[must_use]
    pub const fn claim(&self) -> RecoveryClaimRef {
        self.claim
    }

    /// Complete original request, binding, plan, control and browser attribution.
    #[must_use]
    pub const fn authorization(&self) -> &RecoveryAuthorized {
        &self.authorization
    }

    /// Exact use covered by the observation.
    #[must_use]
    pub const fn purpose(&self) -> &RecoverySubjectUse {
        &self.purpose
    }
}

/// One original session's independently supplied availability during this operation.
#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryAvailability {
    /// The original confirmed session and effective grants remain available.
    Live,
    /// An actual session or effective-grant check established loss.
    EstablishedUnavailable,
    /// The check supplied no established answer.
    Indeterminate,
}

#[derive(Debug)]
struct Observation<'op> {
    key: &'op RecoverySubjectKey,
    availability: RecoveryAvailability,
}

/// Borrowed observations for one machine invocation, without an append permit.
#[derive(Debug)]
pub struct RecoveryOperation<'op> {
    observations: &'op [Observation<'op>],
    invariant: PhantomData<fn(&'op ()) -> &'op ()>,
}

impl RecoveryOperation<'_> {
    /// Explicitly supply no browser authority for this invocation.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            observations: &[],
            invariant: PhantomData,
        }
    }

    pub(crate) fn availability_for(&self, key: &RecoverySubjectKey) -> RecoveryAvailability {
        let mut found = None;
        for observation in self.observations.iter().filter(|entry| entry.key == key) {
            if found.is_some_and(|prior| prior != &observation.availability) {
                return RecoveryAvailability::Indeterminate;
            }
            found = Some(&observation.availability);
        }
        match found {
            Some(RecoveryAvailability::Live) => RecoveryAvailability::Live,
            Some(RecoveryAvailability::EstablishedUnavailable) => {
                RecoveryAvailability::EstablishedUnavailable
            }
            Some(RecoveryAvailability::Indeterminate) | None => RecoveryAvailability::Indeterminate,
        }
    }

    pub(crate) fn unavailable_subjects(&self) -> impl Iterator<Item = &RecoverySubjectKey> {
        self.observations
            .iter()
            .filter(|entry| entry.availability == RecoveryAvailability::EstablishedUnavailable)
            .map(|entry| entry.key)
    }
}

/// Observe detached historical subjects afresh and borrow the answers for one callback.
/// The callback cannot retain an operation or a borrowed observation.
///
/// ```compile_fail
/// use dudes_supervisor::{RecoveryAvailability, with_recovery_operation};
/// let retained = with_recovery_operation(&[], |_| RecoveryAvailability::Live, |operation| operation);
/// ```
pub fn with_recovery_operation<T>(
    subjects: &[RecoverySubjectKey],
    mut observe: impl FnMut(&RecoverySubjectKey) -> RecoveryAvailability,
    use_once: impl for<'op> FnOnce(&RecoveryOperation<'op>) -> T,
) -> T {
    let observations: Vec<_> = subjects
        .iter()
        .map(|key| Observation {
            key,
            availability: observe(key),
        })
        .collect();
    use_once(&RecoveryOperation {
        observations: &observations,
        invariant: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::record::Hex32;
    use dudes_proto::recovery::{
        RecoveryAttribution, RecoveryBinding, RecoveryContext, RecoveryEpisodeId, RecoveryPlan,
        RecoveryRecap, RecoveryWork,
    };
    use dudes_types::{AgentId, GoalId, Lsn, ModelSelection, Provider, TurnId};

    fn key() -> RecoverySubjectKey {
        let work = RecoveryWork {
            agent: AgentId::from_parts(1, 1),
            goal: GoalId::from_parts(1, 2),
            turn: TurnId::from_parts(1, 3),
        };
        let episode = RecoveryEpisodeId {
            work,
            origin: Lsn(3),
        };
        let authorization = RecoveryAuthorized {
            episode,
            binding: RecoveryBinding {
                work,
                hired_lsn: Lsn(1),
                issue_lsn: Lsn(2),
                ticket: None,
                worktree: None,
                provider: Provider::Claude,
                model_selection: Some(ModelSelection::provider_default()),
            },
            request_id: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            attribution: RecoveryAttribution::BrowserSession {
                session_digest: Hex32::new([3; 32]),
            },
            expected_control: None,
            resulting_control: None,
            resume_lsn: None,
            plan: RecoveryPlan::New {
                turn: TurnId::from_parts(1, 4),
                context: RecoveryContext::Fresh {
                    session: None,
                    recap: RecoveryRecap::default(),
                },
            },
        };
        RecoverySubjectKey::new(
            RecoveryClaimRef {
                episode,
                authorization_lsn: Lsn(4),
            },
            authorization,
            RecoverySubjectUse::AdmitNew,
        )
    }

    #[test]
    fn empty_and_absent_observations_never_establish_authority_or_loss() {
        let key = key();
        assert_eq!(
            RecoveryOperation::empty().availability_for(&key),
            RecoveryAvailability::Indeterminate
        );
        assert_eq!(RecoveryOperation::empty().unavailable_subjects().count(), 0);
        with_recovery_operation(
            &[],
            |_| panic!("empty subjects must not be observed"),
            |operation| {
                assert_eq!(
                    operation.availability_for(&key),
                    RecoveryAvailability::Indeterminate
                );
                assert_eq!(operation.unavailable_subjects().count(), 0);
            },
        );
    }

    #[test]
    fn one_operation_observes_each_exact_subject_independently() {
        let first = key();
        let mut second = first.clone();
        second.authorization.request_digest = Hex32::new([99; 32]);
        with_recovery_operation(
            &[first.clone(), second.clone()],
            |key| {
                if key == &first {
                    RecoveryAvailability::Live
                } else {
                    RecoveryAvailability::EstablishedUnavailable
                }
            },
            |operation| {
                assert_eq!(
                    operation.availability_for(&first),
                    RecoveryAvailability::Live
                );
                assert_eq!(
                    operation.availability_for(&second),
                    RecoveryAvailability::EstablishedUnavailable
                );
                assert_eq!(operation.unavailable_subjects().count(), 1);
            },
        );
    }

    #[test]
    fn conflicting_duplicate_answers_are_indeterminate() {
        let key = key();
        let mut first = true;
        with_recovery_operation(
            &[key.clone(), key.clone()],
            |_| {
                if std::mem::take(&mut first) {
                    RecoveryAvailability::Live
                } else {
                    RecoveryAvailability::EstablishedUnavailable
                }
            },
            |operation| {
                assert_eq!(
                    operation.availability_for(&key),
                    RecoveryAvailability::Indeterminate
                );
            },
        );
    }
}
