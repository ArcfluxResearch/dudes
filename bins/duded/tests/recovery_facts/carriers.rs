//! Both folds preserve a recovery allocation against independent valid issue carriers.

#[path = "../../../../crates/dudes-journal/tests/support/recovery_carriers.rs"]
mod fixture;

use super::*;
use fixture::Carrier;

fn ordinary(world: &World, turn: TurnId) -> (Option<Lsn>, String) {
    (
        world.last_applied_lsn,
        format!(
            "{:?}",
            (
                &world.agents,
                &world.goals,
                &world.turns,
                world.browser_notes(),
                world.browser_issue(turn)
            )
        ),
    )
}

fn allocation(case: Carrier) {
    use dudes_proto::recovery::RecoveryPlan;
    for reuse in [false, true] {
        let (mut prefix, authorization, _) = recovery_new_prefix();
        let RecoveryPlan::New { turn: reserved, .. } = authorization.plan else {
            unreachable!()
        };
        assert_ne!(reserved, fixture::unreserved_turn());
        let mut candidate = fixture::candidate(case, authorization.binding.work.agent, |record| {
            prefix.recovery_append(record)
        });
        assert!(!prefix.world.turns.contains_key(&reserved));
        assert!(!prefix.world.turns.contains_key(&fixture::unreserved_turn()));
        if !reuse {
            prefix.recovery_append(candidate);
            assert_eq!(
                prefix
                    .world
                    .turns
                    .get(&fixture::unreserved_turn())
                    .unwrap()
                    .agent,
                fixture::owner()
            );
            let receipt = prefix
                .writer
                .checked_recovery(&mut prefix.persistent)
                .unwrap()
                .receipt(authorization.request_id, authorization.request_digest)
                .unwrap()
                .unwrap();
            assert_eq!(
                receipt.state,
                dudes_proto::recovery_state::RecoveryClaimState::Pending
            );
            continue;
        }
        fixture::set_turn(&mut candidate, reserved);
        let kind = candidate.kind();
        let ordinary_before = ordinary(&prefix.world, reserved);
        let recovery_before = prefix
            .world
            .recovery_projection()
            .map(|(state, history)| (state.clone(), history.clone()))
            .unwrap();
        let before = prefix.persistent.export().unwrap();
        let bytes = journal_bytes(&prefix.dir.path().join("journal"));
        let attempted = prefix.writer.next_lsn().unwrap();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: candidate,
        };
        let result = prefix
            .writer
            .checked_recovery(&mut prefix.persistent)
            .unwrap()
            .append(&entry);
        let refusal = match result {
            Err(Error::RecoveryAdmission { kind, reason }) => Some((kind, reason)),
            Err(other) => panic!("unexpected allocation refusal for {case:?}: {other}"),
            Ok(_) => None,
        };
        let world_result = prefix.world.apply_checked(attempted, &entry);
        let observed = (
            refusal,
            world_result,
            prefix.persistent.export().unwrap() == before,
            journal_bytes(&prefix.dir.path().join("journal")) == bytes,
            prefix.world.recovery_projection().unwrap() == (&recovery_before.0, &recovery_before.1),
            ordinary(&prefix.world, reserved) == ordinary_before,
        );
        assert_eq!(
            observed,
            (
                Some((kind, RecoveryStateError::Binding)),
                Err(RecoveryStateError::Binding),
                true,
                true,
                true,
                true
            ),
            "{case:?}"
        );
    }
}

#[test]
fn an_independent_conversation_preserves_recovery_allocation_in_both_folds() {
    allocation(Carrier::Conversation);
}
#[test]
fn an_independent_completed_note_preserves_recovery_allocation_in_both_folds() {
    allocation(Carrier::Note);
}
#[test]
fn an_independent_taught_note_preserves_recovery_allocation_in_both_folds() {
    allocation(Carrier::TaughtNote);
}
#[test]
fn an_independent_mail_wake_preserves_recovery_allocation_in_both_folds() {
    allocation(Carrier::MailWake);
}
