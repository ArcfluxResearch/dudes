use super::*;
use crate::codex_profile::scoped::Attempt;
use crate::runtime::scoped_launch::ScopedReservation;

#[test]
fn scoped_reservations_block_consent_and_exempt_only_the_exact_dispatch_attempt() {
    let mut fixture = ScopedFixture::new();
    fixture.daemon.ctx.host.inner =
        common::SharedHost::with_native_evidence(fixture.setup.fixture.journal_dir());
    assert!(consent_clear(&fixture));
    let completion = fixture.prepare();
    let attempt = completion.attempt;
    assert!(!consent_clear(&fixture));
    assert_dispatch(&fixture, attempt, true);
    assert_dispatch(
        &fixture,
        Attempt {
            generation: attempt.generation + 1,
            ..attempt
        },
        false,
    );
    let other = Attempt {
        turn: TurnId::from_parts(90, 1),
        owner: InstructionObservationOwner::Managed {
            agent: fixture.issued.agent,
            turn: TurnId::from_parts(90, 1),
            issue: InstructionIssueAssociation::Prospective,
        },
        ..attempt
    };
    fixture
        .daemon
        .ctx
        .scoped_launches
        .insert(other.turn, other.into());
    assert_dispatch(&fixture, attempt, false);
    fixture.daemon.ctx.scoped_launches.remove(&other.turn);
    fixture.daemon.ctx.scoped_launches.insert(
        attempt.turn,
        ScopedReservation::Ready {
            browser_generation: 41,
            prepared: Box::new(completion.prepared.unwrap()),
            outbox: completion.outbox,
        },
    );
    assert!(!consent_clear(&fixture));
    assert_dispatch(&fixture, attempt, true);
    assert!(
        fixture
            .daemon
            .ctx
            .scoped_browser_candidate(&fixture.daemon.sup, attempt.turn)
            .is_none()
    );
    fixture.daemon.ctx.resolved_browser_turn = Some((attempt.turn, 42));
    assert!(
        fixture
            .daemon
            .ctx
            .scoped_browser_candidate(&fixture.daemon.sup, attempt.turn)
            .is_none()
    );
    fixture.daemon.ctx.resolved_browser_turn = Some((attempt.turn, 41));
    assert_eq!(
        fixture
            .daemon
            .ctx
            .scoped_browser_candidate(&fixture.daemon.sup, attempt.turn),
        Some(attempt)
    );
    fixture.daemon.ctx.resolved_browser_turn = None;
    fixture.daemon.ctx.scoped_launches.insert(
        attempt.turn,
        ScopedReservation::Dispatching {
            attempt,
            browser_generation: Some(41),
        },
    );
    assert!(!consent_clear(&fixture));
    assert_dispatch(&fixture, attempt, true);
    fixture.daemon.ctx.scoped_launches.clear();
    assert!(consent_clear(&fixture));
    assert_eq!(fixture.host.spawn_calls(), 0);
}

fn consent_clear(fixture: &ScopedFixture) -> bool {
    let checkout = fixture
        .daemon
        .ctx
        .recovery_consent_checkout_clear(&fixture.daemon.sup, fixture.issued.agent);
    let prior = fixture
        .daemon
        .ctx
        .admit_recovery_prior_attempt(
            &fixture.daemon.sup,
            fixture.issued.agent,
            Some(fixture.issued.turn),
        )
        .is_ok();
    assert_eq!(checkout, prior);
    checkout
}

fn assert_dispatch(fixture: &ScopedFixture, candidate: Attempt, allowed: bool) {
    assert_eq!(
        fixture.daemon.ctx.recovery_checkout_clear_for(
            &fixture.daemon.sup,
            fixture.issued.agent,
            Some(candidate)
        ),
        allowed,
        "checkout census"
    );
    assert_eq!(
        fixture
            .daemon
            .ctx
            .admit_prior_attempt_with_candidate(
                &fixture.daemon.sup,
                fixture.issued.agent,
                Some(fixture.issued.turn),
                Some(candidate)
            )
            .is_ok(),
        allowed,
        "prior attempt census"
    );
}
