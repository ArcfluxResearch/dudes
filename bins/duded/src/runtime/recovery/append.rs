use super::{CommandContext, Ctx, NoWrite, permit_refusal};
use crate::floor_session::{FloorRecoveryAuthority, MutationPermit, RecoveryGrants};
use crate::predicate::PredicateRunner;
use dudes_journal::projections::recovery::{CheckedClaimSubject, CheckedWorkSubject};
use dudes_proto::recovery::RecoveryWork;
use dudes_proto::recovery_state::RecoveryClaimState;
use dudes_proto::{Entry, Record};
use dudes_supervisor::input::{PersistToken, RecoveryAppend, RecoveryIssueAppend, RecoveryRefusal};
use dudes_supervisor::{Clock, ProcessHost, Rng, Supervisor};

pub(in crate::runtime) struct CheckedIssue {
    pub(in crate::runtime) parked: RecoveryIssueAppend,
    checked: Box<CheckedClaimSubject>,
}

pub(in crate::runtime) enum IssueClassification {
    Ordinary,
    Held,
    Recovery(Box<CheckedIssue>),
}

#[derive(Clone, Copy)]
pub(in crate::runtime) enum AppendSubject<'a> {
    Command {
        context: &'a CommandContext,
        metadata: &'a RecoveryAppend,
    },
    NewIssue(&'a CheckedIssue),
}

impl AppendSubject<'_> {
    pub(in crate::runtime) fn join(&self, entry: &Entry) -> Result<(), NoWrite> {
        match self {
            Self::Command { context, metadata } => context
                .join(metadata, entry)
                .map_err(|_| NoWrite(RecoveryRefusal::Unavailable)),
            Self::NewIssue(issue) => {
                if issued_work(entry).is_none_or(|work| {
                    work != issue.checked.queried_work() || work.turn != issue.parked.turn()
                }) || !pending_matches(&issue.parked, &issue.checked)
                {
                    return Err(NoWrite(RecoveryRefusal::Unavailable));
                }
                Ok(())
            }
        }
    }

    pub(in crate::runtime) fn permit<'a>(
        &'a self,
        authority: Option<&'a FloorRecoveryAuthority>,
        grants: RecoveryGrants,
    ) -> Result<Option<MutationPermit<'a>>, NoWrite> {
        match self {
            Self::Command { context, .. } => context.permit(authority, grants),
            Self::NewIssue(issue) => match issue.checked.receipt().authorization.attribution {
                dudes_proto::recovery::RecoveryAttribution::LocalOperator { .. } => Ok(None),
                origin @ dudes_proto::recovery::RecoveryAttribution::BrowserSession { .. } => {
                    if !grants.enabled() {
                        return Err(NoWrite(RecoveryRefusal::AuthorityUnavailable));
                    }
                    authority
                        .ok_or(NoWrite(RecoveryRefusal::Unavailable))?
                        .permit_original(&origin, grants)
                        .map(Some)
                        .map_err(|error| NoWrite(permit_refusal(error)))
                }
            },
        }
    }
}

fn issued_work(entry: &Entry) -> Option<RecoveryWork> {
    match &entry.record {
        Record::TurnIssued {
            agent, goal, turn, ..
        }
        | Record::TurnIssuedWithBrowserNote {
            agent, goal, turn, ..
        } => Some(RecoveryWork {
            agent: *agent,
            goal: *goal,
            turn: *turn,
        }),
        _ => None,
    }
}

fn pending_matches(parked: &RecoveryIssueAppend, checked: &CheckedClaimSubject) -> bool {
    let receipt = checked.receipt();
    receipt.claim == parked.subject().claim()
        && receipt.authorization == *parked.subject().authorization()
        && matches!(receipt.state, RecoveryClaimState::Pending)
        && checked.current_facts().is_some()
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(in crate::runtime) fn classify_recovery_issue<R: Rng>(
        &mut self,
        sup: &Supervisor<super::super::Shared<C>, R>,
        token: PersistToken,
        entry: &Entry,
    ) -> IssueClassification {
        let Ok(parked) = sup.recovery_issue_append(token, entry) else {
            return IssueClassification::Held;
        };
        let Some(work) = issued_work(entry) else {
            return IssueClassification::Ordinary;
        };
        let checked = self
            .writer
            .checked_recovery(&mut self.projections)
            .and_then(|store| store.authority_subjects(&[work]));
        match (parked, checked) {
            (None, Ok(subjects)) if matches!(subjects.as_slice(), [CheckedWorkSubject::NoClaim { work: actual, .. }] if *actual == work) => {
                IssueClassification::Ordinary
            }
            (Some(parked), Ok(mut subjects)) if subjects.len() == 1 => {
                let CheckedWorkSubject::Claim(checked) = subjects.remove(0) else {
                    return IssueClassification::Held;
                };
                if pending_matches(&parked, &checked) {
                    IssueClassification::Recovery(Box::new(CheckedIssue { parked, checked }))
                } else {
                    IssueClassification::Held
                }
            }
            _ => IssueClassification::Held,
        }
    }
}
