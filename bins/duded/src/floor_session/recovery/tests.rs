use super::*;
use crate::floor_session::{FloorCarry, SessionIdentity};
use dudes_supervisor::testing::FakeClock;
use std::sync::atomic::AtomicBool;

const HOST: &str = "127.0.0.1:43123";
const ORIGIN: &str = "http://127.0.0.1:43123";
const GRANTS: RecoveryGrants = RecoveryGrants {
    recovery: true,
    pause_resume: true,
};

fn new_sessions(clock: &Arc<FakeClock>) -> FloorSessions {
    FloorSessions::new(HOST.parse().unwrap(), Arc::clone(clock))
}

fn sign_in(sessions: &FloorSessions) -> (SessionIdentity, super::super::Exchange) {
    let bootstrap = sessions.issue().unwrap();
    let exchange = sessions
        .exchange(HOST, ORIGIN, bootstrap.bootstrap.as_bytes())
        .unwrap();
    let identity = sessions
        .authorize_onboard(HOST, ORIGIN, &exchange.cookie, &exchange.csrf)
        .unwrap();
    (identity, exchange)
}

fn attribution(identity: &SessionIdentity) -> RecoveryAttribution {
    RecoveryAttribution::BrowserSession {
        session_digest: identity.digest(),
    }
}

#[test]
fn original_confirmed_recovery_observations_and_permits_never_refresh_idle() {
    let clock = Arc::new(FakeClock::frozen(0));
    let sessions = new_sessions(&clock);
    let (identity, _) = sign_in(&sessions);
    let authority = sessions.recovery_authority();
    let origin = attribution(&identity);
    for _ in 0..3 {
        clock.advance(100);
        assert_eq!(
            authority.observe_original(&origin, GRANTS),
            RecoveryAvailability::Live
        );
        drop(authority.permit_original(&origin, GRANTS).unwrap());
        drop(
            authority
                .permit_identity(&identity, &origin, GRANTS)
                .unwrap(),
        );
        let state = sessions.0.registry.lock();
        let row = &state.sessions[&identity.cookie];
        assert_eq!(row.created, dudes_types::Timestamp::UNIX_EPOCH);
        assert_eq!(row.last_used, dudes_types::Timestamp::UNIX_EPOCH);
    }
    clock.advance(super::super::IDLE_EXPIRES_MS - 300);
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::EstablishedUnavailable
    );
    assert!(authority.permit_original(&origin, GRANTS).is_err());
    assert!(identity.recovery_permit(GRANTS).is_err());
}

#[test]
fn recovery_requires_both_effective_grants_and_the_actual_original_identity() {
    let clock = Arc::new(FakeClock::frozen(0));
    let sessions = new_sessions(&clock);
    let (identity, _) = sign_in(&sessions);
    let (other, _) = sign_in(&sessions);
    let authority = sessions.recovery_authority();
    let origin = attribution(&identity);
    for grants in [
        RecoveryGrants {
            recovery: false,
            pause_resume: false,
        },
        RecoveryGrants {
            recovery: true,
            pause_resume: false,
        },
        RecoveryGrants {
            recovery: false,
            pause_resume: true,
        },
    ] {
        assert_eq!(
            authority.observe_original(&origin, grants),
            RecoveryAvailability::EstablishedUnavailable
        );
        assert!(
            authority
                .permit_identity(&identity, &origin, grants)
                .is_err()
        );
    }
    assert!(authority.permit_identity(&other, &origin, GRANTS).is_err());
    let stale_revocation = SessionIdentity {
        revoked: Arc::new(AtomicBool::new(false)),
        ..identity.clone()
    };
    assert!(stale_revocation.recovery_permit(GRANTS).is_err());
    let different_registry = new_sessions(&clock);
    let (foreign, _) = sign_in(&different_registry);
    assert!(
        authority
            .permit_identity(&foreign, &attribution(&foreign), GRANTS)
            .is_err()
    );
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::Live
    );
    identity.revoked.store(true, Ordering::Release);
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::EstablishedUnavailable
    );
    assert!(identity.recovery_permit(GRANTS).is_err());
}

#[test]
fn imported_original_is_unavailable_until_confirmed_in_the_current_registry() {
    let clock = Arc::new(FakeClock::frozen(0));
    let old = new_sessions(&clock);
    let (identity, exchange) = sign_in(&old);
    let origin = attribution(&identity);
    let old_carry = FloorCarry::new(old.clone(), None, crate::baseline::CheckRuns::default());
    let exported = old_carry.export(Some("old"));
    old.invalidate();
    let current = new_sessions(&clock);
    let carry = FloorCarry::new(current.clone(), None, crate::baseline::CheckRuns::default());
    assert_eq!(carry.import(Some("new"), &exported), 1);
    let authority = current.recovery_authority();
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::EstablishedUnavailable
    );
    assert!(
        authority
            .permit_identity(&identity, &origin, GRANTS)
            .is_err()
    );
    let imported = {
        let state = current.0.registry.lock();
        let row = &state.sessions[&identity.cookie];
        assert!(row.csrf.is_none());
        SessionIdentity {
            inner: Arc::clone(&current.0),
            cookie: identity.cookie,
            revoked: Arc::clone(&row.revoked),
        }
    };
    drop(imported.permit().unwrap());
    assert!(imported.recovery_permit(GRANTS).is_err());
    let recovered = current.recover(HOST, ORIGIN, &exchange.cookie).unwrap();
    let confirmed = current
        .authorize_onboard(HOST, ORIGIN, &exchange.cookie, &recovered.csrf)
        .unwrap();
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::Live
    );
    drop(
        authority
            .permit_identity(&confirmed, &origin, GRANTS)
            .unwrap(),
    );
    assert!(
        authority
            .permit_identity(&identity, &origin, GRANTS)
            .is_err()
    );
    current.invalidate();
    assert_eq!(
        authority.observe_original(&origin, GRANTS),
        RecoveryAvailability::EstablishedUnavailable
    );
    assert!(confirmed.recovery_permit(GRANTS).is_err());
}
