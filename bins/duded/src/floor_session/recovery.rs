use dudes_proto::recovery::RecoveryAttribution;
use dudes_supervisor::RecoveryAvailability;

use super::{FloorSessions, Inner, MutationPermit, Session, SessionIdentity, registry};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy)]
pub(crate) struct RecoveryGrants {
    pub(crate) recovery: bool,
    pub(crate) pause_resume: bool,
}

impl RecoveryGrants {
    pub(crate) const fn enabled(self) -> bool {
        self.recovery && self.pause_resume
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryPermitFailure {
    AuthorityUnavailable,
    Indeterminate,
}

impl std::fmt::Display for RecoveryPermitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AuthorityUnavailable => "original Floor recovery authority is unavailable",
            Self::Indeterminate => "original Floor recovery authority could not be established",
        })
    }
}

impl std::error::Error for RecoveryPermitFailure {}

#[derive(Clone)]
pub(crate) struct FloorRecoveryAuthority(Arc<Inner>);

impl FloorSessions {
    pub(crate) fn recovery_authority(&self) -> FloorRecoveryAuthority {
        FloorRecoveryAuthority(Arc::clone(&self.0))
    }
}

impl FloorRecoveryAuthority {
    pub(crate) fn observe_original(
        &self,
        origin: &RecoveryAttribution,
        grants: RecoveryGrants,
    ) -> RecoveryAvailability {
        match self.permit_original(origin, grants) {
            Ok(permit) => {
                drop(permit);
                RecoveryAvailability::Live
            }
            Err(RecoveryPermitFailure::AuthorityUnavailable) => {
                RecoveryAvailability::EstablishedUnavailable
            }
            Err(RecoveryPermitFailure::Indeterminate) => RecoveryAvailability::Indeterminate,
        }
    }

    pub(crate) fn permit_original(
        &self,
        origin: &RecoveryAttribution,
        grants: RecoveryGrants,
    ) -> Result<MutationPermit<'_>, RecoveryPermitFailure> {
        let RecoveryAttribution::BrowserSession { session_digest } = origin else {
            return Err(RecoveryPermitFailure::Indeterminate);
        };
        let registry::Locked { state, now } = self.0.registry.lock_at_now();
        if !grants.enabled() || !self.0.valid.load(Ordering::Acquire) {
            return Err(RecoveryPermitFailure::AuthorityUnavailable);
        }
        let row = state.sessions.get(&session_digest.bytes());
        if row.is_none_or(|row| !confirmed_live(row, now)) {
            return Err(RecoveryPermitFailure::AuthorityUnavailable);
        }
        Ok(MutationPermit { _guard: state })
    }

    pub(crate) fn permit_identity<'a>(
        &self,
        identity: &'a SessionIdentity,
        origin: &RecoveryAttribution,
        grants: RecoveryGrants,
    ) -> Result<MutationPermit<'a>, RecoveryPermitFailure> {
        if !Arc::ptr_eq(&self.0, &identity.inner)
            || *origin
                != (RecoveryAttribution::BrowserSession {
                    session_digest: identity.digest(),
                })
        {
            return Err(RecoveryPermitFailure::AuthorityUnavailable);
        }
        identity.recovery_permit(grants)
    }
}

impl SessionIdentity {
    pub(crate) fn recovery_permit(
        &self,
        grants: RecoveryGrants,
    ) -> Result<MutationPermit<'_>, RecoveryPermitFailure> {
        let registry::Locked { state, now } = self.inner.registry.lock_at_now();
        if !grants.enabled()
            || !self.inner.valid.load(Ordering::Acquire)
            || self.revoked.load(Ordering::Acquire)
            || state.sessions.get(&self.cookie).is_none_or(|row| {
                !Arc::ptr_eq(&row.revoked, &self.revoked) || !confirmed_live(row, now)
            })
        {
            return Err(RecoveryPermitFailure::AuthorityUnavailable);
        }
        Ok(MutationPermit { _guard: state })
    }
}

fn confirmed_live(row: &Session, now: dudes_types::Timestamp) -> bool {
    row.csrf.is_some() && !row.revoked.load(Ordering::Acquire) && row.live_at(now)
}

#[cfg(test)]
mod tests;
