//! Contracts for live instruction admission and positioned durable validation.

use std::path::Path;

use dudes_journal::segment::{list_segments, segment_path};
use dudes_journal::{Error, JournalOptions, Projections, Writer, open, replay};
use dudes_proto::instruction_state::InstructionStateError;
use dudes_proto::record::samples;
use dudes_proto::{Entry, InstructionObservationOwner, Record, RecordKind};
use dudes_types::{Timestamp, TurnId};

fn prefix() -> (tempfile::TempDir, Writer, Projections, Entry) {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(dir.path(), JournalOptions::default()).unwrap();
    let mut projections = Projections::in_memory().unwrap();
    let mut observation = samples::record(RecordKind::InstructionObservationStarted);
    let Record::InstructionObservationStarted { ticket, .. } = &mut observation else {
        unreachable!()
    };
    *ticket = None;
    let observation = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: observation,
    };
    for entry in [
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(RecordKind::AgentHired),
        },
        observation.clone(),
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(RecordKind::TurnIssued),
        },
    ] {
        let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
        projections.apply(lsn, crc, &entry).unwrap();
    }
    (dir, writer, projections, observation)
}

fn journal_bytes(dir: &Path) -> Vec<(u32, Vec<u8>)> {
    list_segments(dir)
        .unwrap()
        .into_iter()
        .map(|segment| (segment, std::fs::read(segment_path(dir, segment)).unwrap()))
        .collect()
}

#[test]
fn live_refusal_names_the_unappended_kind_and_preserves_the_committed_prefix() {
    let (dir, _writer, projections, refused) = prefix();
    let before = projections.export().unwrap();
    let bytes = journal_bytes(dir.path());
    let error = projections.validate_next_instruction(&refused).unwrap_err();
    assert_eq!(
        error.to_string(),
        "journal instruction record InstructionObservationStarted refused before append: invalid instruction history: Boundary"
    );
    assert_eq!(projections.export().unwrap(), before);
    assert_eq!(journal_bytes(dir.path()), bytes);

    let mut fresh = refused;
    let Record::InstructionObservationStarted {
        owner: InstructionObservationOwner::Managed { turn, .. },
        ..
    } = &mut fresh.record
    else {
        unreachable!()
    };
    *turn = TurnId::from_parts(1, 99);
    projections.validate_next_instruction(&fresh).unwrap();
    assert_eq!(projections.export().unwrap(), before);
    assert_eq!(journal_bytes(dir.path()), bytes);
}

#[test]
fn durable_instruction_failures_keep_the_offending_position() {
    let (_dir, mut writer, mut projections, refused) = prefix();
    let (lsn, crc) = writer.append_witnessed(&refused).unwrap();
    let error = projections.apply(lsn, crc, &refused).unwrap_err();
    assert!(matches!(
        error,
        Error::Instruction {
            lsn: found,
            reason: InstructionStateError::Boundary,
        } if found == lsn
    ));
    let mut rebuilt = Projections::in_memory().unwrap();
    let error = replay(&writer.reader(), &mut rebuilt).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("journal instruction record at {lsn}: invalid instruction history: Boundary")
    );
    assert!(matches!(
        error,
        Error::Instruction {
            lsn: found,
            reason: InstructionStateError::Boundary,
        } if found == lsn
    ));
}
