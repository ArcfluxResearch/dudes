//! An independent valid owner cannot reuse a recovery reservation's turn identity.

#[path = "../../../../../tests/support/recovery_carriers.rs"]
mod fixture;

use super::*;
use fixture::Carrier;

fn allocation(case: Carrier) {
    for reuse in [false, true] {
        let (mut fixture, authorization, _) = fresh(0);
        let RecoveryPlan::New { turn: reserved, .. } = authorization.plan else {
            unreachable!()
        };
        assert_ne!(reserved, fixture::unreserved_turn());
        let mut candidate = fixture::candidate(case, authorization.binding.work.agent, |record| {
            append(&mut fixture, record)
        });
        assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
        assert!(fixture.projections.turn(reserved).unwrap().is_none());
        assert!(
            fixture
                .projections
                .turn(fixture::unreserved_turn())
                .unwrap()
                .is_none()
        );
        if !reuse {
            append(&mut fixture, candidate);
            assert_eq!(
                fixture
                    .projections
                    .turn(fixture::unreserved_turn())
                    .unwrap()
                    .unwrap()
                    .agent,
                fixture::owner()
            );
            assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
            assert_canonical(&mut fixture);
            continue;
        }
        fixture::set_turn(&mut candidate, reserved);
        let kind = candidate.kind();
        let before = fixture.projections.export().unwrap();
        let bytes = journal_inventory(&fixture);
        let result = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap()
            .append(&Entry {
                at: Timestamp::UNIX_EPOCH,
                record: candidate,
            });
        let refusal = match result {
            Err(Error::RecoveryAdmission { kind, reason }) => Some((kind, reason)),
            Err(other) => panic!("unexpected allocation refusal for {case:?}: {other}"),
            Ok(_) => None,
        };
        let observed = (
            refusal,
            fixture.projections.export().unwrap() == before,
            journal_inventory(&fixture) == bytes,
            fixture.projections.turn(reserved).unwrap().is_none(),
            receipt(&mut fixture).state,
        );
        assert_eq!(
            observed,
            (
                Some((kind, RecoveryStateError::Binding)),
                true,
                true,
                true,
                RecoveryClaimState::Pending
            ),
            "{case:?}"
        );
    }
}

#[test]
fn a_valid_conversation_cannot_reuse_another_agents_reserved_turn() {
    allocation(Carrier::Conversation);
}
#[test]
fn a_valid_completed_note_cannot_reuse_another_agents_reserved_turn() {
    allocation(Carrier::Note);
}
#[test]
fn a_valid_taught_note_cannot_reuse_another_agents_reserved_turn() {
    allocation(Carrier::TaughtNote);
}
#[test]
fn a_valid_mail_wake_cannot_reuse_another_agents_reserved_turn() {
    allocation(Carrier::MailWake);
}
