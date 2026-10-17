use super::*;
use crate::runtime::Witness;
use crate::runtime::work_test_support as common;
use std::sync::Arc;

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn empty(fixture: &common::Fixture) -> TestDaemon {
    let settings = fixture.settings();
    crate::boot(
        &settings,
        common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap()
}

#[test]
fn request_only_absence_is_inert_and_names_the_checked_cut() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let before = daemon.ctx.projections.export().unwrap();
    let entries = fixture.entries();
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let through = daemon.ctx.projections.last_applied_lsn();
    let request = Hex32::new([71; 32]);
    for _ in 0..2 {
        assert_eq!(
            daemon.ctx.recovery_read(request),
            RecoveryReceiptResult::Absent {
                checked_through: through
            }
        );
    }
    assert_eq!(daemon.ctx.projections.export().unwrap(), before);
    assert_eq!(fixture.entries(), entries);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(daemon.sup.effects().is_empty());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn lookup_refuses_raw_journal_advancement_without_certifying_absence() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let request = Hex32::new([72; 32]);
    assert!(matches!(
        daemon.ctx.recovery_read(request),
        RecoveryReceiptResult::Absent { .. }
    ));
    let before = daemon.ctx.projections.export().unwrap();
    let position = daemon.ctx.writer.next_lsn().unwrap();
    let entry = dudes_proto::Entry {
        at: dudes_types::Timestamp::UNIX_EPOCH,
        record: dudes_proto::Record::Checkpoint {
            projection_version: 51,
            segment: position.segment(),
            offset: position.offset(),
        },
    };
    daemon.ctx.writer.append(&entry).unwrap();
    let entries = fixture.entries();
    assert_eq!(
        daemon.ctx.recovery_read(request),
        RecoveryReceiptResult::Unavailable {
            reason: RecoveryReadUnavailable::StaleCut
        }
    );
    assert_eq!(daemon.ctx.projections.export().unwrap(), before);
    assert_eq!(fixture.entries(), entries);
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn only_preappend_refusals_are_known_not_to_have_written() {
    let clean = anyhow::Error::new(dudes_journal::Error::RecoveryAdmission {
        kind: dudes_proto::RecordKind::RecoveryAuthorized,
        reason: RecoveryStateError::Control,
    });
    assert_eq!(known_no_write(&clean), Some(RecoveryRefusal::StaleControl));
    let durable = anyhow::Error::new(dudes_journal::Error::RecoveryRecord {
        lsn: dudes_types::Lsn(17),
        reason: RecoveryStateError::Control,
    });
    assert_eq!(known_no_write(&durable), None);
    let uncertain = anyhow::Error::new(dudes_journal::Error::RecoveryStore(
        dudes_journal::error::RecoveryStoreError::JournalUncertain,
    ));
    assert_eq!(known_no_write(&uncertain), None);
}
