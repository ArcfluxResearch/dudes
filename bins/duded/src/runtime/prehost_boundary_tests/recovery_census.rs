use super::*;

#[test]
fn scoped_preparation_remains_visible_to_recovery_consent() {
    let mut fixture = ScopedFixture::new();
    fixture.daemon.ctx.host.inner =
        common::SharedHost::with_native_evidence(fixture.setup.fixture.journal_dir());
    assert!(
        fixture
            .daemon
            .ctx
            .recovery_consent_checkout_clear(&fixture.daemon.sup, fixture.issued.agent)
    );
    let _completion = fixture.prepare();
    assert!(
        !fixture
            .daemon
            .ctx
            .recovery_consent_checkout_clear(&fixture.daemon.sup, fixture.issued.agent)
    );
    assert_eq!(fixture.host.spawn_calls(), 0);
}
