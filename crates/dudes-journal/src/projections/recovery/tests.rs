use super::*;
use crate::writer::{Fault, Fsync, JournalOptions};
use dudes_proto::RecordKind;
use dudes_proto::record::samples;
use dudes_types::Timestamp;

struct Fixture {
    dir: tempfile::TempDir,
    writer: Writer,
    projections: Projections,
}

impl Fixture {
    fn new(checkpoints: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at: 16 * 1024 * 1024,
                checkpoint_every: checkpoints,
            },
        )
        .unwrap();
        writer.append(&entry(RecordKind::AgentHired)).unwrap();
        let mut projections =
            Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        crate::replay(&writer.reader(), &mut projections).unwrap();
        Self {
            dir,
            writer,
            projections,
        }
    }
}

fn entry(kind: RecordKind) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record: samples::record(kind),
    }
}

fn request() -> Hex32 {
    Hex32::new([7; 32])
}
fn digest() -> Hex32 {
    Hex32::new([8; 32])
}

fn reason(error: Error) -> RecoveryStoreError {
    let Error::RecoveryStore(reason) = error else {
        panic!("expected fixed checked-store refusal: {error}");
    };
    reason
}

#[test]
fn ordinary_prefix_receipt_reads_are_inert_and_do_not_resurvey_history() {
    let mut fixture = Fixture::new(0);
    let before = fixture.projections.export().unwrap();
    let gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let counts = crate::reader::validation_counts();
    for _ in 0..4 {
        assert_eq!(gateway.receipt(request(), digest()).unwrap(), None);
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    drop(gateway);
    assert_eq!(fixture.projections.export().unwrap(), before);
}

#[test]
fn checked_append_and_canonical_ordinary_replay_have_identical_exports() {
    let mut fixture = Fixture::new(0);
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let appended = gateway.append(&entry(RecordKind::Pause)).unwrap();
    assert_eq!(gateway.receipt(request(), digest()).unwrap(), None);
    drop(gateway);
    assert_eq!(fixture.projections.last_applied_lsn(), appended);
    let mut rebuilt = Projections::in_memory().unwrap();
    crate::replay(&fixture.writer.reader(), &mut rebuilt).unwrap();
    assert_eq!(
        fixture.projections.export().unwrap(),
        rebuilt.export().unwrap()
    );
}

#[test]
fn checked_append_projects_the_actual_automatic_checkpoint_too() {
    let mut fixture = Fixture::new(1);
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let main = gateway.append(&entry(RecordKind::Pause)).unwrap();
    assert_eq!(gateway.receipt(request(), digest()).unwrap(), None);
    drop(gateway);
    let frames: Vec<_> = fixture
        .writer
        .reader()
        .witnessed_from(main)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(frames.len(), 2);
    assert!(matches!(frames[1].2.record, Record::Checkpoint { .. }));
    assert_eq!(fixture.projections.last_applied_lsn(), frames[1].0);
    assert_eq!(
        fixture.projections.last_applied_crc(),
        Some(u64::from(frames[1].1))
    );
}

#[test]
fn raw_journal_advance_cannot_turn_a_stale_cut_into_a_not_found_receipt() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    fixture.writer.append(&entry(RecordKind::Pause)).unwrap();
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(error), RecoveryStoreError::JournalAdvanced);
}

#[test]
fn raw_projection_mutation_revokes_the_certificate_and_requires_canonical_equality() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    let fake = Lsn::from_parts(2, 0).unwrap();
    fixture
        .projections
        .apply(fake, 123, &entry(RecordKind::Pause))
        .unwrap();
    assert!(fixture.projections.recovery_certification.is_none());
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn a_failed_raw_projection_batch_also_revokes_certification() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    let record = entry(RecordKind::Pause);
    let first = Lsn::from_parts(2, 10).unwrap();
    let second = Lsn::from_parts(2, 0).unwrap();
    let before = fixture.projections.export().unwrap();
    assert!(
        fixture
            .projections
            .apply_batch([(first, 1, &record), (second, 2, &record)])
            .is_err()
    );
    assert!(fixture.projections.recovery_certification.is_none());
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert!(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .is_ok()
    );
}

#[test]
fn a_failed_main_write_leaves_projection_rows_unchanged_and_receipts_unavailable() {
    let mut fixture = Fixture::new(0);
    let before = fixture.projections.export().unwrap();
    fixture.writer.fail_next(Fault::WriteShort(5));
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    assert!(gateway.append(&entry(RecordKind::Pause)).is_err());
    assert_eq!(
        reason(gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    drop(gateway);
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert!(fixture.projections.recovery_certification.is_none());
}

#[test]
fn an_ok_main_append_with_a_poisoned_checkpoint_cannot_certify_a_receipt() {
    let mut fixture = Fixture::new(1);
    let before = fixture.projections.export().unwrap();
    let main = fixture.writer.next_lsn().unwrap();
    fixture.writer.fail_second_write(Fault::WriteShort(5));
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    assert_eq!(
        reason(gateway.append(&entry(RecordKind::Pause)).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    drop(gateway);
    let first = fixture
        .writer
        .reader()
        .witnessed_from(main)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(first.2, entry(RecordKind::Pause));
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert!(fixture.projections.recovery_certification.is_none());
}

#[test]
fn a_coherently_deleted_ordinary_witness_subtree_requires_canonical_comparison() {
    let mut fixture = Fixture::new(0);
    let txn = fixture.projections.db.begin_write().unwrap();
    txn.delete_table(RECOVERY).unwrap();
    txn.open_table(RECOVERY).unwrap();
    txn.commit().unwrap();
    let read = fixture.projections.db.begin_read().unwrap();
    let table = read.open_table(RECOVERY).unwrap();
    assert!(
        RecoveryCache::read_all(&CacheReader(&table), fixture.projections.last_applied).is_ok()
    );
    drop(table);
    drop(read);
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn full_board_rows_are_compared_even_when_the_public_card_view_is_unchanged() {
    let mut fixture = Fixture::new(0);
    let created = entry(RecordKind::CardCreated);
    let (at, crc) = fixture.writer.append_witnessed(&created).unwrap();
    fixture.projections.apply(at, crc, &created).unwrap();
    let public_before = fixture.projections.cards().unwrap();
    let txn = fixture.projections.db.begin_write().unwrap();
    {
        let mut board = txn.open_table(super::super::BOARD).unwrap();
        let key = public_before[0].id.to_bytes();
        let mut row: serde_json::Value =
            serde_json::from_slice(board.get(&key).unwrap().unwrap().value()).unwrap();
        row.as_object_mut()
            .unwrap()
            .insert("claimed_lsn".into(), serde_json::json!(Lsn(17)));
        board
            .insert(&key, serde_json::to_vec(&row).unwrap().as_slice())
            .unwrap();
    }
    txn.commit().unwrap();
    assert_eq!(fixture.projections.cards().unwrap(), public_before);
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn a_certificate_cannot_be_transferred_to_another_owned_store_generation() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    let mut other = Projections::in_memory().unwrap();
    crate::replay(&fixture.writer.reader(), &mut other).unwrap();
    other.recovery_certification = fixture.projections.recovery_certification.take();
    let error = fixture.writer.checked_recovery(&mut other).err().unwrap();
    assert_eq!(reason(error), RecoveryStoreError::Uncertified);
}

#[test]
fn reopen_discards_certification_and_rechecks_the_actual_prefix() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    let mut reopened = Projections::open_with(&path, Fsync::None).unwrap();
    assert!(reopened.recovery_certification.is_none());
    assert!(fixture.writer.checked_recovery(&mut reopened).is_ok());
}

#[test]
fn literal_version_47_cache_rebuilds_the_new_recovery_witness_table() {
    let mut fixture = Fixture::new(0);
    let expected = fixture.projections.export().unwrap();
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    {
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(SCHEMA_VERSION, &47u64)
            .unwrap();
        txn.delete_table(RECOVERY).unwrap();
        txn.commit().unwrap();
    }
    let mut reopened = Projections::open_with(&path, Fsync::None).unwrap();
    assert_eq!(reopened.last_applied_lsn(), Lsn::ZERO);
    assert!(reopened.recovery_certification.is_none());
    crate::replay(&fixture.writer.reader(), &mut reopened).unwrap();
    assert_eq!(reopened.export().unwrap(), expected);
    assert!(fixture.writer.checked_recovery(&mut reopened).is_ok());
}

#[test]
fn altered_metadata_cannot_reuse_an_existing_certification() {
    let mut fixture = Fixture::new(0);
    drop(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap(),
    );
    let txn = fixture.projections.db.begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(LAST_APPLIED_CRC, &99u64)
        .unwrap();
    txn.commit().unwrap();
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(error), RecoveryStoreError::Metadata);
    assert!(fixture.projections.recovery_certification.is_none());
}

fn journal_bytes(
    directory: &std::path::Path,
) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[test]
fn mail_wake_preappend_refusal_has_no_durable_position_and_changes_no_bytes() {
    let mut fixture = Fixture::new(0);
    let attempted = mail_wake_missing_completion(&fixture.projections);
    let end = fixture.writer.next_lsn().unwrap();
    let journal = fixture.dir.path().join("journal");
    let before_journal = journal_bytes(&journal);
    let before_cache = fixture.projections.export().unwrap();
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let error = gateway.append(&attempted).unwrap_err();
    let Error::MailWakeAdmission { kind, reason } = error else {
        panic!("expected preappend refusal: {error}");
    };
    assert_eq!(kind, RecordKind::MailWakeTurnDecided);
    assert_eq!(reason, dudes_proto::mail_wake::MailWakeError::Completion);
    drop(gateway);
    assert_eq!(fixture.writer.next_lsn().unwrap(), end);
    assert_eq!(fixture.writer.poisoned(), None);
    assert!(!fixture.writer.recovery_fenced);
    assert_eq!(journal_bytes(&journal), before_journal);
    assert_eq!(fixture.projections.export().unwrap(), before_cache);
}

fn mail_wake_missing_completion(projections: &Projections) -> Entry {
    let mut attempted = entry(RecordKind::MailWakeTurnDecided);
    let Record::MailWakeTurnDecided(decision) = &mut attempted.record else {
        unreachable!()
    };
    decision.completion_lsn = projections.last_applied_lsn();
    attempted
}

#[test]
fn recorded_mail_wake_history_refusal_keeps_its_actual_position() {
    let mut fixture = Fixture::new(0);
    let attempted = mail_wake_missing_completion(&fixture.projections);
    let actual = fixture.writer.append(&attempted).unwrap();
    let error = fixture
        .writer
        .reader()
        .validate_completed_contracts()
        .err()
        .unwrap();
    let Error::MailWake { lsn, reason } = error else {
        panic!("expected recorded history refusal: {error}");
    };
    assert_eq!(lsn, actual);
    assert_eq!(reason, dudes_proto::mail_wake::MailWakeError::Completion);
}
