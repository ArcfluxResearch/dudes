//! Required repository instruction headers refuse before replay can change any cache.

use dudes_journal::{Error, Reader, frame, segment};
use dudes_proto::record::{Entry, RecordKind, samples};
use dudes_types::Timestamp;

#[test]
fn instruction_batches_require_exact_headers_on_both_detection_paths() {
    for kind in [
        RecordKind::InstructionObservationBatchStarted,
        RecordKind::InstructionObservationBatchFinished,
    ] {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(kind),
        };
        let payload = serde_json::to_vec(&entry).unwrap();
        for header_kind in [kind.tag(), RecordKind::CardCreated.tag()] {
            let dir = tempfile::tempdir().unwrap();
            let mut bytes = Vec::new();
            frame::encode(header_kind, &payload, &mut bytes).unwrap();
            let path = segment::segment_path(dir.path(), 1);
            std::fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                Reader::open(dir.path()),
                Err(Error::RepositoryFrame { .. } | Error::InstructionFrame { .. })
            ));
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
}
