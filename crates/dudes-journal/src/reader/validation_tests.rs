use super::*;
use crate::{Fsync, JournalOptions, Projections, Writer, open, replay};
use dudes_proto::record::{Hex32, samples};
use dudes_proto::{
    Gate, GateStep, LoopDecision, MailFrom, MailRef, MailTo, MailWakeAdmission, MailWakeDecision,
    MailWakeDigest, MailWakeDigestDomain, MailWakePlan, MailWakePurpose, MailWakeTarget,
    MailWakeTask, Record,
};
use dudes_types::{MailState, Timestamp};

fn options() -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 0,
        ..JournalOptions::default()
    }
}

fn completed(dir: &Path) -> (Writer, Lsn, Entry) {
    let (mut writer, _) = open(dir, options()).unwrap();
    let mut append = |record| {
        writer
            .append(&Entry {
                at: Timestamp::UNIX_EPOCH,
                record,
            })
            .unwrap()
    };
    let hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired { id: agent, .. } = hired else {
        unreachable!();
    };
    append(hired);
    let goal = samples::record(RecordKind::GoalSet);
    let Record::GoalSet { goal: spec, .. } = &goal else {
        unreachable!();
    };
    let goal_id = spec.id;
    append(goal);
    append(samples::record(RecordKind::PredicateResult));
    let completion_lsn = append(Record::LoopDecided {
        agent,
        input_digest: "completed".into(),
        gate_trace: Vec::new(),
        decision: LoopDecision::Done,
    });
    let mut mail = samples::record(RecordKind::Mail);
    let Record::Mail {
        id,
        from,
        to,
        state,
        ..
    } = &mut mail
    else {
        unreachable!();
    };
    let id = *id;
    *from = MailFrom::Human;
    *to = MailTo::Agent { agent };
    *state = MailState::Durable;
    let reference = MailRef {
        id,
        lsn: append(mail),
    };
    let task = MailWakeTask::new(1).unwrap();
    let root = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::MailWakeTurnDecided(MailWakeDecision {
            agent,
            goal: goal_id,
            completion_lsn,
            purpose: MailWakePurpose::FreshPreparation {
                plan: MailWakePlan {
                    trigger: reference,
                    frontier: reference.lsn,
                    unread_count: 1,
                    task: task.clone(),
                    targets: vec![MailWakeTarget {
                        mail: reference,
                        original_count: 0,
                        target_count: 1,
                    }],
                },
            },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Fresh,
                value: Hex32::new([1; 32]),
            },
            gate_trace: Gate::ORDER
                .into_iter()
                .map(|gate| GateStep {
                    gate,
                    fired: gate == Gate::Work,
                    note: None,
                })
                .collect(),
            decision: MailWakeAdmission::Issue { task },
        }),
    };
    let lsn = writer.append(&root).unwrap();
    (writer, lsn, root)
}

#[test]
fn reader_open_and_replay_share_one_completed_contract_pass() {
    let dir = tempfile::tempdir().unwrap();
    drop(completed(dir.path()));
    VALIDATION_PASSES.set((0, 0));
    let (reader, _) = Reader::open(dir.path()).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (1, 1));
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (2, 1));
}

#[test]
fn writer_handoff_reuses_validation_but_a_fresh_reader_validates() {
    let dir = tempfile::tempdir().unwrap();
    drop(completed(dir.path()));
    VALIDATION_PASSES.set((0, 0));
    let (writer, _) = open(dir.path(), options()).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (1, 1));
    let mut projections = Projections::in_memory().unwrap();
    replay(&writer.reader(), &mut projections).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (2, 1));
    replay(&Reader::new(dir.path()), &mut projections).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (3, 2));
}

#[test]
fn legacy_journal_never_runs_a_completed_contract_fold() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(dir.path(), options()).unwrap();
    writer
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(RecordKind::AgentHired),
        })
        .unwrap();
    drop(writer);
    VALIDATION_PASSES.set((0, 0));
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (2, 0));
}

#[test]
fn appended_invalid_contract_cannot_reuse_an_earlier_prefix_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _, root) = completed(dir.path());
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    let before = projections.export().unwrap();
    writer.append(&root).unwrap();
    VALIDATION_PASSES.set((0, 0));
    assert!(matches!(
        replay(&reader, &mut projections),
        Err(Error::MailWake { .. })
    ));
    assert_eq!(VALIDATION_PASSES.get(), (1, 1));
    assert_eq!(projections.export().unwrap(), before);
}

#[test]
fn writer_reader_revalidates_a_contract_appended_after_writer_open() {
    let dir = tempfile::tempdir().unwrap();
    let (writer, _, root) = completed(dir.path());
    drop(writer);
    let (mut writer, _) = open(dir.path(), options()).unwrap();
    let reader = writer.reader();
    writer.append(&root).unwrap();
    let mut projections = Projections::in_memory().unwrap();
    VALIDATION_PASSES.set((0, 0));
    assert!(matches!(
        replay(&reader, &mut projections),
        Err(Error::MailWake { .. })
    ));
    assert_eq!(VALIDATION_PASSES.get(), (1, 1));
    assert_eq!(projections.last_applied_lsn(), Lsn::ZERO);
}

#[test]
fn same_length_rewrite_cannot_reuse_an_earlier_prefix_receipt() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let (writer, lsn, mut root) = completed(dir.path());
    drop(writer);
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    let before = projections.export().unwrap();
    let original_len = serde_json::to_vec(&root).unwrap().len();
    let Record::MailWakeTurnDecided(decision) = &mut root.record else {
        unreachable!()
    };
    decision.completion_lsn.0 += 1;
    let payload = serde_json::to_vec(&root).unwrap();
    assert_eq!(payload.len(), original_len);
    let mut bytes = Vec::new();
    frame::encode_required(root.record.kind(), &payload, &mut bytes).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .open(segment_path(dir.path(), lsn.segment()))
        .unwrap();
    file.seek(SeekFrom::Start(lsn.offset())).unwrap();
    file.write_all(&bytes).unwrap();
    VALIDATION_PASSES.set((0, 0));
    assert!(matches!(
        replay(&reader, &mut projections),
        Err(Error::MailWake { .. })
    ));
    assert_eq!(VALIDATION_PASSES.get(), (1, 1));
    assert_eq!(projections.export().unwrap(), before);
}

#[test]
fn replay_snapshot_excludes_frames_appended_after_its_validation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _, root) = completed(dir.path());
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let validated = reader.validate_completed_contracts().unwrap();
    let expected: Vec<_> = validated
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let appended = writer.append(&root).unwrap();
    assert_eq!(validated.locate(appended).unwrap(), Location::Missing);
    assert_eq!(
        validated
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn repaired_torn_tail_does_not_repeat_completed_contract_validation() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let (writer, lsn, _) = completed(dir.path());
    drop(writer);
    OpenOptions::new()
        .append(true)
        .open(segment_path(dir.path(), lsn.segment()))
        .unwrap()
        .write_all(&[1, 2, 3])
        .unwrap();
    VALIDATION_PASSES.set((0, 0));
    let (writer, report) = open(dir.path(), options()).unwrap();
    assert!(report.torn.is_some());
    let mut projections = Projections::in_memory().unwrap();
    replay(&writer.reader(), &mut projections).unwrap();
    assert_eq!(VALIDATION_PASSES.get(), (2, 1));
}

fn source_files(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort();
    files
}

fn storage_child(journal: &Path, scratch: &Path, operation: &str, expected: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "reader::validation_tests::temporary_storage_child",
            "--nocapture",
        ])
        .env("TMPDIR", scratch)
        .env("TMP", scratch)
        .env("TEMP", scratch)
        .env("DUDES_VALIDATION_TEST_JOURNAL", journal)
        .env("DUDES_VALIDATION_TEST_SCRATCH", scratch)
        .env("DUDES_VALIDATION_TEST_OPERATION", operation)
        .env("DUDES_VALIDATION_TEST_EXPECTED", expected)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn temporary_storage_child() {
    let Some(journal) = std::env::var_os("DUDES_VALIDATION_TEST_JOURNAL") else {
        return;
    };
    let journal = Path::new(&journal);
    let operation = std::env::var("DUDES_VALIDATION_TEST_OPERATION").unwrap();
    let result = match operation.as_str() {
        "reader" => Reader::open(journal).map(|_| ()),
        "writer" => open(journal, options()).map(|_| ()),
        "writer_with_scratch" => {
            let scratch = journal.parent().unwrap();
            let (mut writer, _) = crate::open_with_scratch(journal, options(), scratch).unwrap();
            let reader = writer.reader();
            writer
                .append(&Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: samples::record(RecordKind::Checkpoint),
                })
                .unwrap();
            let mut cache = Projections::in_memory().unwrap();
            replay(&reader, &mut cache).map(|_| ())
        }
        "replay" => {
            let mut cache = Projections::in_memory().unwrap();
            cache
                .apply(
                    Lsn(1),
                    7,
                    &Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record: samples::record(RecordKind::AgentHired),
                    },
                )
                .unwrap();
            let before = cache.export().unwrap();
            let result = replay(&Reader::new(journal), &mut cache).map(|_| ());
            assert_eq!(cache.export().unwrap(), before);
            result
        }
        _ => unreachable!(),
    };
    let expected = std::env::var("DUDES_VALIDATION_TEST_EXPECTED").unwrap();
    if let Some(step) = expected.strip_prefix("scratch:") {
        let scratch = std::env::var_os("DUDES_VALIDATION_TEST_SCRATCH").unwrap();
        assert_scratch_refusal(&result.unwrap_err(), Path::new(&scratch), step);
        return;
    }
    match expected.as_str() {
        "ok" => result.unwrap(),
        "io" => assert!(
            matches!(result, Err(Error::ValidationScratch { source, .. }) if matches!(source.as_ref(), Error::Io(_)))
        ),
        "overlap" => assert!(
            matches!(result, Err(Error::ValidationScratch { source, .. }) if matches!(source.as_ref(), Error::Io(error) if error.kind() == io::ErrorKind::InvalidInput))
        ),
        "semantic" => assert!(matches!(result, Err(Error::MailWake { .. })), "{result:?}"),
        _ => unreachable!(),
    }
}

fn assert_scratch_refusal(error: &Error, scratch: &Path, step: &str) {
    let message = error.to_string();
    assert!(message.contains("validation scratch"), "{message}");
    assert!(
        message.contains(&scratch.display().to_string()),
        "{message}"
    );
    assert!(message.contains(step), "{message}");
    assert!(message.contains("retry"), "{message}");
    assert!(!message.contains("delete state.redb"), "{message}");
}

#[test]
fn temporary_storage_refusal_names_directory_step_and_recovery() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    drop(completed(&journal));
    let before = source_files(&journal);
    let missing = root.path().join("missing");
    let file = root.path().join("file");
    fs::write(&file, b"not a directory").unwrap();
    let file_refusal = if cfg!(windows) {
        "scratch:resolve scratch directory"
    } else {
        "scratch:create anonymous file"
    };
    for (scratch, expected) in [
        (&missing, "scratch:resolve scratch directory"),
        (&file, file_refusal),
        (&journal, "scratch:check journal isolation"),
    ] {
        for operation in ["reader", "writer", "replay"] {
            storage_child(&journal, scratch, operation, expected);
            assert_eq!(source_files(&journal), before);
        }
    }
    assert!(!missing.exists());
    assert_eq!(fs::read(&file).unwrap(), b"not a directory");
}

#[cfg(unix)]
#[test]
fn read_only_scratch_refusal_names_directory_before_repair_or_cache_changes() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    let scratch = root.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let (writer, lsn, _) = completed(&journal);
    drop(writer);
    OpenOptions::new()
        .append(true)
        .open(segment_path(&journal, lsn.segment()))
        .unwrap()
        .write_all(&[1, 2, 3])
        .unwrap();
    let before = source_files(&journal);
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o500)).unwrap();
    for operation in ["reader", "writer", "replay"] {
        storage_child(
            &journal,
            &scratch,
            operation,
            "scratch:create anonymous file",
        );
        assert_eq!(source_files(&journal), before);
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    }
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn writer_scratch_selection_survives_reader_revalidation_with_unusable_ambient_temp() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    drop(completed(&journal));
    storage_child(
        &journal,
        &root.path().join("missing-ambient-temp"),
        "writer_with_scratch",
        "ok",
    );
}

#[cfg(unix)]
#[test]
fn validation_file_is_private_before_database_writes() {
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = tempfile::tempdir().unwrap();
    let file = validation_file(scratch.path(), scratch.path()).unwrap();
    let metadata = file.metadata().unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.len(), 0);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn validation_database_failure_names_scratch_without_cache_recovery() {
    let scratch = tempfile::tempdir().unwrap();
    let file = tempfile::NamedTempFile::new_in(scratch.path()).unwrap();
    let error = validation_projection_from_file(File::open(file.path()).unwrap(), scratch.path())
        .err()
        .unwrap();
    assert_scratch_refusal(&error, scratch.path(), "initialize validation database");
}

#[test]
fn temporary_storage_is_cleaned_after_acceptance_and_semantic_refusal() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    let scratch = root.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let (mut writer, _, invalid) = completed(&journal);
    let before = source_files(&journal);
    storage_child(&journal, &scratch, "reader", "ok");
    assert_eq!(source_files(&journal), before);
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    writer.append(&invalid).unwrap();
    drop(writer);
    let before = source_files(&journal);
    storage_child(&journal, &scratch, "reader", "semantic");
    assert_eq!(source_files(&journal), before);
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
}

#[test]
fn temporary_storage_failure_precedes_repair_and_cache_changes() {
    use std::io::Write as _;
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    let (writer, lsn, _) = completed(&journal);
    drop(writer);
    let segment = segment_path(&journal, lsn.segment());
    OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(&[1, 2, 3])
        .unwrap();
    let before = source_files(&journal);
    let missing = root.path().join("missing");
    let file = root.path().join("file");
    fs::write(&file, b"not a directory").unwrap();
    let case_alias = journal.with_file_name("JOURNAL");
    for scratch in [&missing, &file, &journal, &case_alias] {
        for operation in ["reader", "writer", "replay"] {
            storage_child(&journal, scratch, operation, "io");
            assert_eq!(source_files(&journal), before);
        }
    }
    assert!(!missing.exists());
    assert_eq!(fs::read(&file).unwrap(), b"not a directory");
}

#[test]
fn validation_file_preserves_projection_results_and_refuses_unwritable_handles() {
    let journal = tempfile::tempdir().unwrap();
    drop(completed(journal.path()));
    let mut memory = Projections::in_memory().unwrap();
    let mut disk = Projections::for_validation(tempfile::tempfile().unwrap()).unwrap();
    for frame in Reader::new(journal.path())
        .witnessed_from(Lsn::ZERO)
        .unwrap()
    {
        let (lsn, crc, entry) = frame.unwrap();
        memory.apply(lsn, crc, &entry).unwrap();
        disk.apply(lsn, crc, &entry).unwrap();
    }
    assert_eq!(disk.export().unwrap(), memory.export().unwrap());
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(Projections::for_validation(File::open(file.path()).unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn completed_validation_accepts_read_only_sources_and_refuses_scratch_aliases() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    let scratch = root.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let (writer, lsn, _) = completed(&journal);
    drop(writer);
    let segment = segment_path(&journal, lsn.segment());
    let before = source_files(&journal);
    fs::set_permissions(&segment, fs::Permissions::from_mode(0o400)).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o500)).unwrap();
    storage_child(&journal, &scratch, "reader", "ok");
    let alias = root.path().join("alias");
    symlink(&journal, &alias).unwrap();
    storage_child(&journal, &alias, "reader", "io");
    assert_eq!(source_files(&journal), before);
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn temporary_storage_refuses_firmlink_source_and_descendant() {
    use std::os::unix::fs::MetadataExt as _;
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    drop(completed(&journal));
    let canonical = journal.canonicalize().unwrap();
    let alias = if let Ok(relative) = canonical.strip_prefix("/System/Volumes/Data") {
        Path::new("/").join(relative)
    } else {
        Path::new("/System/Volumes/Data").join(canonical.strip_prefix("/").unwrap())
    };
    let source = journal.metadata().unwrap();
    let alternate = alias.metadata().unwrap();
    assert_eq!(
        (source.dev(), source.ino()),
        (alternate.dev(), alternate.ino())
    );
    let before = source_files(&journal);
    let nested = journal.join("scratch");
    fs::create_dir(&nested).unwrap();
    for scratch in [alias.clone(), alias.join("scratch")] {
        for operation in ["reader", "writer", "replay"] {
            storage_child(&journal, &scratch, operation, "overlap");
            assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
        }
    }
    fs::remove_dir(&nested).unwrap();
    assert_eq!(source_files(&journal), before);
}
