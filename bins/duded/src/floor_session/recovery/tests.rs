use super::*;
use crate::floor_session::SessionIdentity;
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
fn a_device_restored_from_the_journal_keeps_its_recovery_original_live_until_it_is_removed() {
    use dudes_proto::record::{FloorDeviceRemoval, Hex32};
    use dudes_supervisor::DeviceRow;

    let clock = Arc::new(FakeClock::frozen(0));
    let old = new_sessions(&clock);
    let (identity, _) = sign_in(&old);
    let origin = attribution(&identity);
    let (device, row) = {
        let state = old.0.registry.lock();
        let session = &state.sessions[&identity.cookie];
        (
            session.id,
            DeviceRow {
                origin: ORIGIN.to_owned(),
                name: "Chrome".to_owned(),
                cookie: Hex32::new(identity.cookie),
                secret: Hex32::new(session.csrf.unwrap()),
                enrolled: session.created,
                first_used: session.first_used,
                last_used: Some(session.last_used),
                acknowledged: None,
                removed: None,
            },
        )
    };
    old.invalidate();

    let current = new_sessions(&clock);
    current.seed(&std::collections::BTreeMap::from([(device, row.clone())]));
    assert_eq!(
        current
            .recovery_authority()
            .observe_original(&origin, GRANTS),
        RecoveryAvailability::Live
    );

    let mut removed = row;
    removed.removed = Some((
        FloorDeviceRemoval::Removed,
        dudes_types::Timestamp::UNIX_EPOCH,
    ));
    let after = new_sessions(&clock);
    after.seed(&std::collections::BTreeMap::from([(device, removed)]));
    assert_eq!(
        after.recovery_authority().observe_original(&origin, GRANTS),
        RecoveryAvailability::EstablishedUnavailable,
        "a removed device no longer vouches for what it started"
    );
}
