mod records;

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
        Self::with_rotation(checkpoints, 16 * 1024 * 1024)
    }

    fn with_rotation(checkpoints: u64, rotate_at: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut writer, _) = crate::open(
            &journal,
            JournalOptions {
                fsync: Fsync::None,
                rotate_at,
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

fn reason(error: &Error) -> RecoveryStoreError {
    let Error::RecoveryStore(reason) = error else {
        panic!("expected fixed checked-store refusal: {error}");
    };
    *reason
}

#[test]
fn ordinary_prefix_receipt_reads_are_inert_and_do_not_resurvey_history() {
    let mut fixture = Fixture::new(0);
    let before = fixture.projections.export().unwrap();
    let journal = fixture.dir.path().join("journal");
    let bytes = journal_bytes(&journal);
    let gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let counts = crate::reader::validation_counts();
    for _ in 0..4 {
        assert_eq!(gateway.receipt(request(), digest()).unwrap(), None);
        assert_eq!(gateway.receipt_by_request(request()).unwrap(), None);
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert_eq!(journal_bytes(&journal), bytes);
}

#[test]
fn request_only_reads_refuse_uncertified_stale_and_altered_metadata_cuts() {
    for expected in [
        RecoveryStoreError::Uncertified,
        RecoveryStoreError::JournalAdvanced,
        RecoveryStoreError::Metadata,
    ] {
        let mut fixture = Fixture::new(0);
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        match expected {
            RecoveryStoreError::Uncertified => gateway.projections.recovery_certification = None,
            RecoveryStoreError::JournalAdvanced => {
                gateway.writer.append(&entry(RecordKind::Pause)).unwrap();
            }
            RecoveryStoreError::Metadata => {
                let txn = gateway.projections.db.begin_write().unwrap();
                txn.open_table(META)
                    .unwrap()
                    .insert(LAST_APPLIED_CRC, &99u64)
                    .unwrap();
                txn.commit().unwrap();
            }
            _ => unreachable!(),
        }
        let export = gateway.projections.export().unwrap();
        let journal = fixture.dir.path().join("journal");
        let bytes = journal_bytes(&journal);
        let counts = crate::reader::validation_counts();
        for _ in 0..2 {
            assert_eq!(
                reason(&gateway.receipt_by_request(request()).unwrap_err()),
                expected
            );
            assert_eq!(
                reason(&gateway.receipt(request(), digest()).unwrap_err()),
                expected
            );
        }
        assert_eq!(crate::reader::validation_counts(), counts);
        assert_eq!(gateway.projections.export().unwrap(), export);
        assert_eq!(journal_bytes(&journal), bytes);
    }
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
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    fixture.writer.append(&entry(RecordKind::Pause)).unwrap();
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&error), RecoveryStoreError::JournalAdvanced);
}

#[test]
fn raw_projection_mutation_revokes_the_certificate_and_requires_canonical_equality() {
    let mut fixture = Fixture::new(0);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
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
    assert_eq!(reason(&error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn a_failed_raw_projection_batch_also_revokes_certification() {
    let mut fixture = Fixture::new(0);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
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
        reason(&gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(&gateway.receipt_by_request(request()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
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
        reason(&gateway.append(&entry(RecordKind::Pause)).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(&gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(&gateway.receipt_by_request(request()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
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
    assert_eq!(reason(&error), RecoveryStoreError::CanonicalMismatch);
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
    assert_eq!(reason(&error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn a_certificate_cannot_be_transferred_to_another_owned_store_generation() {
    let mut fixture = Fixture::new(0);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let mut other = Projections::in_memory().unwrap();
    crate::replay(&fixture.writer.reader(), &mut other).unwrap();
    other.recovery_certification = fixture.projections.recovery_certification.take();
    let error = fixture.writer.checked_recovery(&mut other).err().unwrap();
    assert_eq!(reason(&error), RecoveryStoreError::Uncertified);
}

#[test]
fn reopen_discards_certification_and_rechecks_the_actual_prefix() {
    let mut fixture = Fixture::new(0);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
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
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
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
    assert_eq!(reason(&error), RecoveryStoreError::Metadata);
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

#[test]
fn checked_append_window_refuses_empty_early_eof_and_truncated_checkpoints() {
    for tail in ["empty", "clean_eof", "partial_header", "partial_payload"] {
        let mut fixture = Fixture::new(1);
        let attempted = entry(RecordKind::Pause);
        let (start, crc) = fixture.writer.append_witnessed(&attempted).unwrap();
        let end = fixture.writer.next_lsn().unwrap();
        let frames = fixture
            .writer
            .reader()
            .witnessed_from(start)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 2);
        let checkpoint = frames[1].0;
        assert_eq!(start.segment(), end.segment());
        let length = match tail {
            "empty" => start.offset(),
            "clean_eof" => checkpoint.offset(),
            "partial_header" => {
                checkpoint.offset() + u64::try_from(crate::frame::HEADER_LEN).unwrap() - 1
            }
            "partial_payload" => end.offset() - 1,
            _ => unreachable!(),
        };
        let path =
            crate::segment::segment_path(&fixture.dir.path().join("journal"), start.segment());
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(length)
            .unwrap();
        let strict = written_interval(&fixture.writer, start, crc, &attempted, end);
        assert!(strict.is_err(), "{tail} was accepted: {strict:?}");
        let live = fixture
            .writer
            .reader()
            .iter_from(start)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(live.len(), usize::from(tail != "empty"));
        if let Some((_, actual)) = live.first() {
            assert_eq!(actual, &attempted);
        }
    }
}

#[derive(Debug)]
struct CommitSyncFault {
    armed: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicUsize,
    observed_journal_advance: std::sync::atomic::AtomicBool,
    journal: std::path::PathBuf,
    before_len: u64,
}

#[derive(Debug)]
struct FailingProjectionSync {
    inner: redb::backends::FileBackend,
    fault: Arc<CommitSyncFault>,
}

impl redb::StorageBackend for FailingProjectionSync {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(offset, bytes)
    }
    fn close(&self) -> std::io::Result<()> {
        self.inner.close()
    }
    fn sync_data(&self) -> std::io::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.fault.armed.load(SeqCst)
            && std::fs::metadata(&self.fault.journal)?.len() > self.fault.before_len
            && self
                .fault
                .armed
                .compare_exchange(true, false, SeqCst, SeqCst)
                .is_ok()
        {
            self.fault.fired.fetch_add(1, SeqCst);
            self.fault.observed_journal_advance.store(true, SeqCst);
            return Err(std::io::Error::other(
                "injected projection commit sync failure",
            ));
        }
        self.inner.sync_data()
    }
    fn try_lock_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<bool, redb::BackendError> {
        self.inner.try_lock_range(start, end)
    }
    fn try_lock_shared_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<bool, redb::BackendError> {
        self.inner.try_lock_shared_range(start, end)
    }
    fn lock_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<(), redb::BackendError> {
        self.inner.lock_range(start, end)
    }
    fn lock_shared_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<(), redb::BackendError> {
        self.inner.lock_shared_range(start, end)
    }
    fn unlock_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<(), redb::BackendError> {
        self.inner.unlock_range(start, end)
    }
    fn query_lock_range(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> Result<bool, redb::BackendError> {
        self.inner.query_lock_range(start, end)
    }
}

fn fault_backed_projection(
    dir: &std::path::Path,
    writer: &Writer,
    state: &std::path::Path,
) -> (Projections, Arc<CommitSyncFault>) {
    let journal = crate::segment::segment_path(&dir.join("journal"), writer.segment());
    let fault = Arc::new(CommitSyncFault {
        armed: std::sync::atomic::AtomicBool::new(false),
        fired: std::sync::atomic::AtomicUsize::new(0),
        observed_journal_advance: std::sync::atomic::AtomicBool::new(false),
        before_len: std::fs::metadata(&journal).unwrap().len(),
        journal,
    });
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(state)
        .unwrap();
    let backend = FailingProjectionSync {
        inner: redb::backends::FileBackend::new(file).unwrap(),
        fault: Arc::clone(&fault),
    };
    let db = redb::Database::builder()
        .create_with_backend(backend)
        .unwrap();
    let projections = Projections::init(db, redb::Durability::Immediate).unwrap();
    (projections, fault)
}

fn record_through_failed_projection_commit(
    writer: &mut Writer,
    projections: &mut Projections,
    fault: &CommitSyncFault,
    attempted: &Entry,
) -> Vec<String> {
    use std::sync::atomic::Ordering::SeqCst;
    let old_cut = projections.last_applied_lsn();
    let old_crc = projections.last_applied_crc();
    let start = writer.next_lsn().unwrap();
    let before = std::fs::read(&fault.journal).unwrap();
    let mut gateway = writer.checked_recovery(projections).unwrap();
    fault.armed.store(true, SeqCst);
    let error = gateway.append(attempted).unwrap_err();
    assert!(
        matches!(&error, Error::Store(redb::Error::Io(_))),
        "unexpected failure: {error}"
    );
    assert_eq!(fault.fired.load(SeqCst), 1);
    assert!(fault.observed_journal_advance.load(SeqCst));
    assert_eq!(gateway.projections.last_applied_lsn(), old_cut);
    assert_eq!(gateway.projections.last_applied_crc(), old_crc);
    assert!(gateway.projections.recovery_certification.is_none());
    assert!(gateway.writer.recovery_fenced);
    assert_eq!(gateway.writer.poisoned(), None);
    let end = gateway.writer.next_lsn().unwrap();
    let frames = gateway
        .writer
        .reader()
        .witnessed_between(start, end)
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, start);
    assert_eq!(&frames[0].2, attempted);
    let after = std::fs::read(&fault.journal).unwrap();
    assert!(after.starts_with(&before));
    assert_eq!(
        reason(&gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(&gateway.receipt_by_request(request()).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(
        reason(&gateway.append(attempted).unwrap_err()),
        RecoveryStoreError::JournalUncertain
    );
    assert_eq!(std::fs::read(&fault.journal).unwrap(), after);
    let mut canonical = Projections::in_memory().unwrap();
    crate::replay(&gateway.writer.reader(), &mut canonical).unwrap();
    assert_eq!(canonical.last_applied_lsn(), start);
    assert_eq!(canonical.last_applied_crc(), Some(u64::from(frames[0].1)));
    authority_rows(&canonical).unwrap()
}

#[test]
fn real_redb_commit_sync_failure_fences_then_reopens_atomically_and_replays() {
    use std::sync::atomic::Ordering::SeqCst;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let options = JournalOptions {
        fsync: Fsync::Full,
        rotate_at: 16 * 1024 * 1024,
        checkpoint_every: 0,
    };
    let (mut writer, _) = crate::open(&journal, options).unwrap();
    writer.append(&entry(RecordKind::AgentHired)).unwrap();
    let (mut projections, fault) =
        fault_backed_projection(dir.path(), &writer, &dir.path().join("state.redb"));
    crate::replay(&writer.reader(), &mut projections).unwrap();
    let old = authority_rows(&projections).unwrap();
    let canonical_new = record_through_failed_projection_commit(
        &mut writer,
        &mut projections,
        &fault,
        &entry(RecordKind::Pause),
    );
    drop(projections);
    drop(writer);
    fault.armed.store(false, SeqCst);
    let (mut writer, _) = crate::open(&journal, options).unwrap();
    let mut reopened = Projections::open(&dir.path().join("state.redb")).unwrap();
    assert!(reopened.recovery_certification.is_none());
    let actual = authority_rows(&reopened).unwrap();
    assert!(
        actual == old || actual == canonical_new,
        "commit exposed mixed projection rows/cursor/witness"
    );
    crate::replay(&writer.reader(), &mut reopened).unwrap();
    assert_eq!(authority_rows(&reopened).unwrap(), canonical_new);
    let gateway = writer.checked_recovery(&mut reopened).unwrap();
    assert_eq!(gateway.receipt(request(), digest()).unwrap(), None);
    assert_eq!(fault.fired.load(SeqCst), 1);
}

#[test]
fn durable_authorization_survives_real_projection_sync_failure_and_request_only_replay() {
    use std::sync::atomic::Ordering::SeqCst;
    let (fixture, authorization) = records::fresh_authorization(0);
    let Fixture {
        dir,
        writer,
        projections,
    } = fixture;
    drop(projections);
    drop(writer);
    let journal = dir.path().join("journal");
    let options = JournalOptions {
        fsync: Fsync::Full,
        rotate_at: 16 * 1024 * 1024,
        checkpoint_every: 0,
    };
    let (mut writer, _) = crate::open(&journal, options).unwrap();
    let state = dir.path().join("fault.redb");
    let (mut projections, fault) = fault_backed_projection(dir.path(), &writer, &state);
    crate::replay(&writer.reader(), &mut projections).unwrap();
    let old = authority_rows(&projections).unwrap();
    let authorization_lsn = writer.next_lsn().unwrap();
    let attempted = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::RecoveryAuthorized(Box::new(authorization.clone())),
    };
    let canonical_new =
        record_through_failed_projection_commit(&mut writer, &mut projections, &fault, &attempted);
    assert_ne!(old, canonical_new);
    drop(projections);
    drop(writer);
    fault.armed.store(false, SeqCst);
    let (mut writer, _) = crate::open(&journal, options).unwrap();
    let mut reopened = Projections::open(&state).unwrap();
    assert!(reopened.recovery_certification.is_none());
    let actual = authority_rows(&reopened).unwrap();
    assert!(
        actual == old || actual == canonical_new,
        "commit exposed mixed authorization rows/cursor/witness"
    );
    let frames = writer
        .reader()
        .iter_from(authorization_lsn)
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(frames, vec![(authorization_lsn, attempted)]);
    crate::replay(&writer.reader(), &mut reopened).unwrap();
    assert_eq!(authority_rows(&reopened).unwrap(), canonical_new);
    let expected = RecoveryReceipt {
        claim: RecoveryClaimRef {
            episode: authorization.episode,
            authorization_lsn,
        },
        authorization,
        state: RecoveryClaimState::Pending,
    };
    let before_rows = reopened.export().unwrap();
    let before_journal = journal_bytes(&journal);
    let before_next = writer.next_lsn().unwrap();
    let gateway = writer.checked_recovery(&mut reopened).unwrap();
    let counts = crate::reader::validation_counts();
    for _ in 0..4 {
        assert_eq!(
            gateway.receipt_by_request(request()).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            gateway.receipt(request(), digest()).unwrap(),
            Some(expected.clone())
        );
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    assert_eq!(gateway.writer.next_lsn().unwrap(), before_next);
    assert_eq!(gateway.projections.export().unwrap(), before_rows);
    assert_eq!(journal_bytes(&journal), before_journal);
    assert_eq!(fault.fired.load(SeqCst), 1);
}

#[test]
fn literal_version_48_cache_rebuilds_factual_positions_and_source_context() {
    let mut fixture = Fixture::new(0);
    let paused = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&entry(RecordKind::Pause))
        .unwrap();
    let resumed = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&entry(RecordKind::Resume))
        .unwrap();
    let expected = fixture.projections.export().unwrap();
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    {
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(SCHEMA_VERSION, &48u64)
            .unwrap();
        {
            let mut agents = txn.open_table(super::super::AGENTS).unwrap();
            let rows = agents
                .iter()
                .unwrap()
                .map(|row| {
                    let (key, value) = row.unwrap();
                    (key.value().to_owned(), value.value().to_vec())
                })
                .collect::<Vec<_>>();
            for (key, value) in rows {
                let mut old: serde_json::Value = serde_json::from_slice(&value).unwrap();
                old.as_object_mut().unwrap().remove("hired_lsn");
                old.as_object_mut().unwrap().remove("session_lsn");
                agents
                    .insert(&key, serde_json::to_vec(&old).unwrap().as_slice())
                    .unwrap();
            }
        }
        {
            let mut recovery = txn.open_table(RECOVERY).unwrap();
            let rows = recovery
                .iter()
                .unwrap()
                .map(|row| {
                    let (key, value) = row.unwrap();
                    (key.value().to_vec(), value.value().to_vec())
                })
                .collect::<Vec<_>>();
            for (key, value) in rows {
                let mut old: serde_json::Value = serde_json::from_slice(&value).unwrap();
                old["value"].as_object_mut().unwrap().remove("context");
                recovery
                    .insert(key.as_slice(), serde_json::to_vec(&old).unwrap().as_slice())
                    .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    let mut reopened = Projections::open_with(&path, Fsync::None).unwrap();
    assert_eq!(reopened.last_applied_lsn(), Lsn::ZERO);
    assert!(reopened.recovery_certification.is_none());
    crate::replay(&fixture.writer.reader(), &mut reopened).unwrap();
    assert_eq!(reopened.export().unwrap(), expected);
    let source = fixture
        .writer
        .checked_recovery(&mut reopened)
        .unwrap()
        .witness(resumed)
        .unwrap();
    assert_eq!(source.context().previous_control.unwrap().lsn, Some(paused));
    assert!(reopened.agents().unwrap()[0].hired_lsn > Lsn::ZERO);
}

#[test]
fn changed_retirement_metadata_cannot_reuse_an_existing_certificate() {
    let mut fixture = Fixture::new(0);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let txn = fixture.projections.db.begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(LAST_RETIREMENT, &17u64)
        .unwrap();
    txn.commit().unwrap();
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&error), RecoveryStoreError::Metadata);
    assert!(fixture.projections.recovery_certification.is_none());
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&error), RecoveryStoreError::CanonicalMismatch);
}

#[test]
fn changed_card_invalidation_position_fails_canonical_equality() {
    let mut fixture = Fixture::new(0);
    let created = entry(RecordKind::CardCreated);
    let (lsn, crc) = fixture.writer.append_witnessed(&created).unwrap();
    fixture.projections.apply(lsn, crc, &created).unwrap();
    let Record::CardCreated { card, .. } = created.record else {
        unreachable!()
    };
    let txn = fixture.projections.db.begin_write().unwrap();
    {
        let mut board = txn.open_table(super::super::BOARD).unwrap();
        let mut row =
            super::super::get_row::<super::super::board::BoardRow>(&board, &card.to_bytes())
                .unwrap()
                .unwrap();
        row.recovery_change_lsn = Lsn(17);
        super::super::put_row(&mut board, &card.to_bytes(), &row).unwrap();
    }
    txn.commit().unwrap();
    let error = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&error), RecoveryStoreError::CanonicalMismatch);
}

fn goal_entries() -> (Entry, Entry, RecoveryBinding) {
    let hired = entry(RecordKind::AgentHired);
    let Record::AgentHired { id, provider, .. } = hired.record else {
        unreachable!()
    };
    let mut installed = entry(RecordKind::GoalSet);
    let Record::GoalSet { agent, goal, .. } = &mut installed.record else {
        unreachable!()
    };
    *agent = id;
    let work = dudes_proto::recovery::RecoveryWork {
        agent: id,
        goal: goal.id,
        turn: dudes_types::TurnId::from_parts(71, 3),
    };
    let issued = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TurnIssued {
            agent: id,
            goal: work.goal,
            turn: work.turn,
            mode: dudes_types::Mode::Managed,
            lsn_of_decision: Lsn::ZERO,
            task: "immutable issued goal task".into(),
            session: None,
            model_selection: Some(dudes_types::ModelSelection::provider_default()),
            answers: Vec::new(),
        },
    };
    (
        installed,
        issued,
        RecoveryBinding {
            work,
            issue_lsn: Lsn::ZERO,
            hired_lsn: Lsn::ZERO,
            ticket: None,
            worktree: None,
            provider,
            model_selection: Some(dudes_types::ModelSelection::provider_default()),
        },
    )
}

fn decision_for(issue: &Entry) -> Entry {
    let Record::TurnIssued { agent, task, .. } = &issue.record else {
        unreachable!()
    };
    let mut decided = entry(RecordKind::LoopDecided);
    if let Record::LoopDecided {
        agent: owner,
        decision,
        ..
    } = &mut decided.record
    {
        *owner = *agent;
        *decision = dudes_proto::LoopDecision::Continue { task: task.clone() };
    }
    decided
}

#[test]
fn before_record_facts_use_prior_entries_in_the_same_uncommitted_transaction() {
    use dudes_proto::recovery::{
        RecoveryBoundary, RecoveryOrigin, RecoveryReason, RecoveryRequired,
    };
    let mut fixture = Fixture::new(0);
    let (installed, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture.projections.last_applied;
    let installed_position = fixture.writer.append_witnessed(&installed).unwrap();
    let decision = decision_for(&issued);
    let decision_position = fixture.writer.append_witnessed(&decision).unwrap();
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision_position.0;
    }
    let issued_position = fixture.writer.append_witnessed(&issued).unwrap();
    binding.issue_lsn = issued_position.0;
    let txn = fixture.projections.db.begin_write().unwrap();
    let folded = super::super::fold_batch(
        &txn,
        fixture.projections.last_applied,
        [
            (installed_position.0, installed_position.1, &installed),
            (decision_position.0, decision_position.1, &decision),
            (issued_position.0, issued_position.1, &issued),
        ],
    )
    .unwrap();
    assert_eq!(folded.last, binding.issue_lsn);
    let tables = FactTables {
        agents: &txn.open_table(super::super::AGENTS).unwrap(),
        goals: &txn.open_table(super::super::GOALS).unwrap(),
        turns: &txn.open_table(super::super::TURNS).unwrap(),
        board: &txn.open_table(super::super::BOARD).unwrap(),
        leases: &txn.open_table(super::super::WORKTREES).unwrap(),
        recovery: &txn.open_table(RECOVERY).unwrap(),
        meta: &txn.open_table(META).unwrap(),
    };
    let facts = facts_at(&tables, folded.last, &binding).unwrap();
    assert_eq!(facts.through, issued_position.0);
    assert_eq!(facts.source_issue.lsn, issued_position.0);
    let required = RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::ThisBarrier,
        reason: RecoveryReason::MissingSpawn,
        boundary: RecoveryBoundary::MissingSpawn,
        predecessor: None,
    };
    prepare_at(
        &tables,
        folded.last,
        RecoveryTransition::Required(&required),
    )
    .unwrap();
    let committed = fixture.projections.db.begin_read().unwrap();
    assert!(
        super::super::get_row::<super::super::TurnRow>(
            &committed.open_table(super::super::TURNS).unwrap(),
            &binding.work.turn.to_bytes()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(fixture.projections.last_applied, binding.hired_lsn);
}

fn erase_version_49_attempt_fields(path: &std::path::Path, turn: dudes_types::TurnId) {
    let db = redb::Database::create(path).unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(SCHEMA_VERSION, &49u64)
        .unwrap();
    let mut turns = txn.open_table(super::super::TURNS).unwrap();
    let key = turn.to_bytes();
    let bytes = turns.get(&key).unwrap().unwrap().value().to_vec();
    let mut old: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for field in ["spawn_lsn", "end_lsn", "confirmation_lsn"] {
        old.as_object_mut().unwrap().remove(field);
    }
    turns
        .insert(&key, serde_json::to_vec(&old).unwrap().as_slice())
        .unwrap();
    drop(turns);
    txn.commit().unwrap();
}

#[test]
fn literal_version_49_cache_rebuilds_latest_attempt_positions_and_context() {
    let mut fixture = Fixture::new(0);
    let (installed, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture.projections.last_applied;
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    gateway.append(&installed).unwrap();
    let decision = gateway.append(&decision_for(&issued)).unwrap();
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    binding.issue_lsn = gateway.append(&issued).unwrap();
    let mut spawned = entry(RecordKind::TurnSpawned);
    if let Record::TurnSpawned {
        agent,
        turn,
        session,
        reissued,
        ..
    } = &mut spawned.record
    {
        *agent = binding.work.agent;
        *turn = binding.work.turn;
        *session = None;
        *reissued = false;
    }
    let spawn = gateway.append(&spawned).unwrap();
    let session = dudes_types::SessionId::new("confirmed-attempt");
    let confirmed = gateway
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::SessionLearned {
                agent: binding.work.agent,
                turn: binding.work.turn,
                session,
            },
        })
        .unwrap();
    let mut ended = entry(RecordKind::TurnEnded);
    if let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome,
        ..
    } = &mut ended.record
    {
        *agent = binding.work.agent;
        *turn = binding.work.turn;
        *mode = dudes_types::Mode::Managed;
        *provenance = dudes_types::Provenance::Observed;
        *outcome = Some(dudes_proto::TurnOutcome::NoResult);
    }
    let end = gateway.append(&ended).unwrap();
    let expected = fixture.projections.export().unwrap();
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    erase_version_49_attempt_fields(&path, binding.work.turn);
    let mut reopened = Projections::open_with(&path, Fsync::None).unwrap();
    assert_eq!(reopened.last_applied_lsn(), Lsn::ZERO);
    crate::replay(&fixture.writer.reader(), &mut reopened).unwrap();
    assert_eq!(reopened.export().unwrap(), expected);
    let gateway = fixture.writer.checked_recovery(&mut reopened).unwrap();
    assert_eq!(
        gateway.facts(&binding).unwrap().attempt,
        RecoveryAttempt {
            spawn_lsn: Some(spawn),
            end_lsn: Some(end),
            confirmation_lsn: Some(confirmed)
        }
    );
    let source = gateway.witness(end).unwrap();
    assert_eq!(
        source.context().attempt.unwrap().issue_lsn,
        binding.issue_lsn
    );
    assert_eq!(
        source.context().attempt.unwrap().previous_spawn_lsn,
        Some(spawn)
    );
}

fn write_literal_version_50_witnesses(path: &std::path::Path) {
    let db = redb::Database::create(path).unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(SCHEMA_VERSION, &50u64)
        .unwrap();
    {
        let mut table = txn.open_table(RECOVERY).unwrap();
        let rows = table
            .iter()
            .unwrap()
            .map(|row| {
                let (key, value) = row.unwrap();
                (key.value().to_vec(), value.value().to_vec())
            })
            .collect::<Vec<_>>();
        let mut standing_removed = 0;
        let mut failures_removed = 0;
        for (key, bytes) in rows {
            let mut cell: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(witness) = cell.get_mut("value") {
                if let Some(previous) = witness
                    .get_mut("context")
                    .and_then(|context| context.get_mut("previous_control"))
                    .and_then(serde_json::Value::as_object_mut)
                {
                    standing_removed += usize::from(previous.remove("standing").is_some());
                }
                if let Some(end) = witness
                    .get_mut("evidence")
                    .and_then(|evidence| evidence.get_mut("End"))
                    .and_then(serde_json::Value::as_object_mut)
                {
                    failures_removed += usize::from(end.remove("fast_failure").is_some());
                }
            }
            table
                .insert(
                    key.as_slice(),
                    serde_json::to_vec(&cell).unwrap().as_slice(),
                )
                .unwrap();
        }
        assert!(standing_removed > 0 && failures_removed > 0);
    }
    txn.commit().unwrap();
}

fn append_qualified_end(
    gateway: &mut CheckedRecoveryStore<'_>,
    work: dudes_proto::recovery::RecoveryWork,
) -> Lsn {
    let mut spawned = entry(RecordKind::TurnSpawned);
    if let Record::TurnSpawned {
        agent,
        turn,
        session,
        ..
    } = &mut spawned.record
    {
        *agent = work.agent;
        *turn = work.turn;
        *session = None;
    }
    gateway.append(&spawned).unwrap();
    let mut ended = entry(RecordKind::TurnEnded);
    if let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        exit,
        elapsed_ms,
        usage,
        model_usage,
        outcome,
        ..
    } = &mut ended.record
    {
        *agent = work.agent;
        *turn = work.turn;
        *mode = dudes_types::Mode::Managed;
        *provenance = dudes_types::Provenance::Observed;
        *exit = dudes_proto::ExitStatus {
            code: Some(1),
            signal: None,
        };
        *elapsed_ms = Some(5_000);
        *usage = None;
        model_usage.clear();
        *outcome = Some(dudes_proto::TurnOutcome::Completed);
    }
    gateway.append(&ended).unwrap()
}

#[test]
fn literal_version_50_cache_rebuilds_standing_control_and_failure_classification() {
    let mut fixture = Fixture::new(0);
    let (installed, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture.projections.last_applied;
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    gateway.append(&installed).unwrap();
    let decision = gateway.append(&decision_for(&issued)).unwrap();
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    binding.issue_lsn = gateway.append(&issued).unwrap();
    let end = append_qualified_end(&mut gateway, binding.work);
    gateway
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Halt {
                agent: binding.work.agent,
                text: None,
            },
        })
        .unwrap();
    gateway
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent: binding.work.agent,
                browser: None,
            },
        })
        .unwrap();
    let resumed = gateway
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Resume {
                agent: binding.work.agent,
                browser: None,
            },
        })
        .unwrap();
    let expected = fixture.projections.export().unwrap();
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    write_literal_version_50_witnesses(&path);
    let mut reopened = Projections::open_with(&path, Fsync::None).unwrap();
    assert_eq!(reopened.last_applied_lsn(), Lsn::ZERO);
    crate::replay(&fixture.writer.reader(), &mut reopened).unwrap();
    assert_eq!(reopened.export().unwrap(), expected);
    let gateway = fixture.writer.checked_recovery(&mut reopened).unwrap();
    assert!(matches!(
        gateway.witness(end).unwrap().evidence(),
        dudes_proto::recovery_state::history::RecoveryEvidence::End {
            fast_failure: true,
            ..
        }
    ));
    let standing = gateway
        .witness(resumed)
        .unwrap()
        .context()
        .previous_control
        .unwrap()
        .standing;
    assert!(standing.halted && standing.paused);
    assert_eq!(
        gateway.facts(&binding).unwrap().control,
        RecoveryStandingControl::default()
    );
}

#[test]
fn guarded_append_returns_reader_frames_for_plain_checkpointed_and_rotating_writes() {
    for (cadence, rotate_at) in [(0, 16 * 1024 * 1024), (1, 16 * 1024 * 1024), (1, 1)] {
        let mut fixture = Fixture::with_rotation(cadence, rotate_at);
        let caller = entry(RecordKind::Pause);
        let committed = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap()
            .append_guarded(&caller, || Ok::<_, ()>(()), || Ok::<_, ()>(()))
            .unwrap();
        let main_lsn = committed.lsn();
        let through = committed.through();
        let (main, checkpoint) = committed.into_parts();
        let actual: Vec<_> = fixture
            .writer
            .reader()
            .witnessed_from(main_lsn)
            .unwrap()
            .map(|frame| {
                let (lsn, _, entry) = frame.unwrap();
                (lsn, entry)
            })
            .collect();
        let supplied: Vec<_> = std::iter::once(main).chain(checkpoint).collect();
        assert_eq!(supplied, actual);
        assert_eq!(supplied[0], (main_lsn, caller));
        assert_eq!(supplied.len(), if cadence == 0 { 1 } else { 2 });
        assert_eq!(through, supplied.last().unwrap().0);
        assert_eq!(fixture.projections.last_applied_lsn(), through);
        if rotate_at == 1 {
            assert_ne!(supplied[0].0.segment(), supplied[1].0.segment());
        }
        let mut replayed = Projections::in_memory().unwrap();
        crate::replay(&fixture.writer.reader(), &mut replayed).unwrap();
        assert_eq!(
            fixture.projections.export().unwrap(),
            replayed.export().unwrap()
        );
    }
}

struct ObservedWriteGuard<'a> {
    events: &'a std::cell::RefCell<Vec<&'static str>>,
    journal: &'a std::path::Path,
    before: &'a std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
}

impl Drop for ObservedWriteGuard<'_> {
    fn drop(&mut self) {
        assert_ne!(
            &journal_bytes(self.journal),
            self.before,
            "commit completed under the guard"
        );
        self.events.borrow_mut().push("drop");
    }
}

#[test]
fn guarded_append_preflights_before_acquire_and_drops_before_the_post_sync_boundary() {
    let mut fixture = Fixture::new(1);
    let journal = fixture.dir.path().join("journal");
    let before = journal_bytes(&journal);
    let events = std::cell::RefCell::new(Vec::new());
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append_guarded(
            &entry(RecordKind::Pause),
            || {
                assert_eq!(journal_bytes(&journal), before);
                events.borrow_mut().push("acquire");
                Ok::<_, ()>(ObservedWriteGuard {
                    events: &events,
                    journal: &journal,
                    before: &before,
                })
            },
            || {
                assert_eq!(*events.borrow(), ["acquire", "drop"]);
                events.borrow_mut().push("after_fsync");
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert_eq!(events.into_inner(), ["acquire", "drop", "after_fsync"]);

    let invalid = mail_wake_missing_completion(&fixture.projections);
    let before = (
        journal_bytes(&journal),
        fixture.projections.export().unwrap(),
    );
    let acquired = std::cell::Cell::new(false);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append_guarded(
            &invalid,
            || {
                acquired.set(true);
                Ok::<_, ()>(())
            },
            || Ok::<_, ()>(()),
        );
    assert!(matches!(
        result,
        Err(GuardedAppendError::Journal(Error::MailWakeAdmission { .. }))
    ));
    assert!(!acquired.get());
    assert_eq!(journal_bytes(&journal), before.0);
    assert_eq!(fixture.projections.export().unwrap(), before.1);
}

#[test]
fn denied_append_guard_changes_no_bytes_and_requires_recertification() {
    let mut fixture = Fixture::new(1);
    let journal = fixture.dir.path().join("journal");
    let before = (
        journal_bytes(&journal),
        fixture.projections.export().unwrap(),
    );
    let next = fixture.writer.next_lsn().unwrap();
    let after = std::cell::Cell::new(false);
    let mut gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let result = gateway.append_guarded(
        &entry(RecordKind::Pause),
        || Err::<(), _>("denied"),
        || {
            after.set(true);
            Ok::<_, ()>(())
        },
    );
    assert!(matches!(result, Err(GuardedAppendError::Acquire("denied"))));
    assert_eq!(
        reason(&gateway.receipt(request(), digest()).unwrap_err()),
        RecoveryStoreError::Uncertified
    );
    assert_eq!(
        reason(&gateway.receipt_by_request(request()).unwrap_err()),
        RecoveryStoreError::Uncertified
    );
    assert!(!after.get());
    assert_eq!(fixture.writer.next_lsn().unwrap(), next);
    assert_eq!(journal_bytes(&journal), before.0);
    assert_eq!(fixture.projections.export().unwrap(), before.1);
    assert!(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .is_ok()
    );
}

#[test]
fn post_sync_failure_retains_the_actual_caller_and_requires_canonical_reopen() {
    let mut fixture = Fixture::new(1);
    let before = fixture.projections.export().unwrap();
    let expected = fixture.writer.next_lsn().unwrap();
    let caller = entry(RecordKind::Pause);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append_guarded(&caller, || Ok::<_, ()>(()), || Err("after"));
    assert!(
        matches!(result, Err(GuardedAppendError::AfterFsync { caller, source: "after" }) if caller == expected)
    );
    let frames: Vec<_> = fixture
        .writer
        .reader()
        .witnessed_from(expected)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!((frames[0].0, &frames[0].2), (expected, &caller));
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert!(fixture.writer.recovery_fenced);
    assert!(
        fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .is_err()
    );
    let Fixture {
        dir,
        writer,
        mut projections,
    } = fixture;
    drop(writer);
    let (mut writer, _) = crate::open(
        &dir.path().join("journal"),
        JournalOptions {
            fsync: Fsync::None,
            rotate_at: 16 * 1024 * 1024,
            checkpoint_every: 1,
        },
    )
    .unwrap();
    crate::replay(&writer.reader(), &mut projections).unwrap();
    assert!(writer.checked_recovery(&mut projections).is_ok());
    let mut canonical = Projections::in_memory().unwrap();
    crate::replay(&writer.reader(), &mut canonical).unwrap();
    assert_eq!(projections.export().unwrap(), canonical.export().unwrap());
}

#[test]
fn a_poisoned_checkpoint_drops_the_guard_without_running_the_post_sync_callback() {
    let mut fixture = Fixture::new(1);
    let journal = fixture.dir.path().join("journal");
    let before = journal_bytes(&journal);
    let cache = fixture.projections.export().unwrap();
    fixture.writer.fail_second_write(Fault::WriteShort(5));
    let events = std::cell::RefCell::new(Vec::new());
    let after = std::cell::Cell::new(false);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append_guarded(
            &entry(RecordKind::Pause),
            || {
                events.borrow_mut().push("acquire");
                Ok::<_, ()>(ObservedWriteGuard {
                    events: &events,
                    journal: &journal,
                    before: &before,
                })
            },
            || {
                after.set(true);
                Err("post-sync must not obscure the poisoned checkpoint")
            },
        );
    assert!(matches!(
        result,
        Err(GuardedAppendError::Journal(Error::RecoveryStore(
            RecoveryStoreError::JournalUncertain
        )))
    ));
    assert_eq!(events.into_inner(), ["acquire", "drop"]);
    assert!(!after.get());
    assert!(fixture.writer.poisoned().is_some());
    assert!(fixture.writer.recovery_fenced);
    assert_eq!(fixture.projections.export().unwrap(), cache);
}
