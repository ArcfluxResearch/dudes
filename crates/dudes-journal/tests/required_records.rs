#![allow(missing_docs)]

use std::fs;

use dudes_journal::frame::{FrameError, Header, crc32c};
use dudes_journal::segment::segment_path;
use dudes_journal::{Error, Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::{Entry, Record, RecordKind};
use dudes_types::{AgentId, Lsn, Timestamp};

fn entry() -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::Resume {
            agent: AgentId::NIL,
            browser: None,
        },
    }
}

fn raw_frame(version: u16, kind: u16, payload: &[u8]) -> Vec<u8> {
    let header = Header {
        len: u32::try_from(payload.len()).unwrap(),
        ver: version,
        kind,
        crc: crc32c(version, kind, payload),
    };
    let mut bytes = header.to_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn mixed_compatible_and_required_records_replay_through_the_public_reader() {
    let dir = tempfile::tempdir().unwrap();
    let expected = entry();
    let payload = serde_json::to_vec(&expected).unwrap();
    let mut bytes = raw_frame(1, 1_000, b"unknown compatible record");
    let position = bytes.len();
    bytes.extend(raw_frame(2, RecordKind::Resume.tag(), &payload));
    fs::write(segment_path(dir.path(), 1), &bytes).unwrap();

    let (reader, report) = Reader::open(dir.path()).expect("both versions are understood");
    assert_eq!(report.frames, 2);
    let entries = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        entries,
        vec![(
            Lsn::from_parts(1, u64::try_from(position).unwrap()).unwrap(),
            expected,
        )]
    );
    let mut store = Projections::in_memory().unwrap();
    let applied = replay(&reader, &mut store).unwrap();
    assert_eq!(applied.applied, 1);
    assert_eq!(replay(&reader, &mut store).unwrap().applied, 0);
    let (_, reopened) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    assert_eq!(reopened.frames, 2);
    assert_eq!(fs::read(segment_path(dir.path(), 1)).unwrap(), bytes);
}

#[test]
fn unknown_required_kinds_refuse_before_reader_or_writer_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = segment_path(dir.path(), 1);
    let bytes = raw_frame(2, 1_000, b"unknown required record");
    fs::write(&path, &bytes).unwrap();
    let reader_error = Reader::open(dir.path()).unwrap_err();
    assert_eq!(
        reader_error.to_string(),
        "journal required record kind 1000 at 1:0 is not understood"
    );
    let writer_error = open(dir.path(), JournalOptions::default()).unwrap_err();
    assert_eq!(writer_error.to_string(), reader_error.to_string());
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

/// a reader that does not know a required kind must refuse
/// the journal before it repairs, rotates or touches a cache, so rolling back to an older release
/// cannot misread records it never understood. Tag 76 stands for the next required kind after the
/// wake, note, worktree and ticket kinds, which an older reader meets in exactly this shape.
#[test]
fn a_future_required_kind_after_understood_history_refuses_both_opens_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    for _ in 0..3 {
        writer.append(&entry()).unwrap();
    }
    drop(writer);
    let path = segment_path(dir.path(), 1);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(&raw_frame(2, 76, b"{\"t\":\"future\"}"));
    fs::write(&path, &bytes).unwrap();
    let cache = dir.path().join("state.redb");
    fs::write(&cache, b"cache witness").unwrap();
    let files = || {
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = files();

    let expected = "journal required record kind 76 at 1:";
    let reader_error = Reader::open(dir.path()).unwrap_err().to_string();
    assert!(reader_error.starts_with(expected), "{reader_error}");
    let writer_error = open(dir.path(), JournalOptions::default())
        .unwrap_err()
        .to_string();
    assert_eq!(writer_error, reader_error);

    let mut advanced = Projections::in_memory().unwrap();
    advanced.apply(Lsn(u64::MAX - 1), 17, &entry()).unwrap();
    let cache_before = advanced.export().unwrap();
    assert!(matches!(
        replay(&Reader::new(dir.path()), &mut advanced),
        Err(Error::RequiredKind { kind: 76, .. })
    ));
    assert_eq!(advanced.export().unwrap(), cache_before);

    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read(&cache).unwrap(), b"cache witness");
    assert_eq!(files(), before);
}

#[test]
fn first_required_append_per_writer_starts_a_fresh_segment() {
    let dir = tempfile::tempdir().unwrap();
    let options = JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 1,
        ..JournalOptions::default()
    };
    let value = entry();
    let (mut writer, _) = open(dir.path(), options).unwrap();
    assert_eq!(
        writer.append(&value).unwrap(),
        Lsn::from_parts(1, 0).unwrap()
    );
    let (first, witness) = writer.append_required_witnessed(&value).unwrap();
    assert_eq!(first, Lsn::from_parts(2, 0).unwrap());
    let bytes = fs::read(segment_path(dir.path(), 2)).unwrap();
    let header = Header::parse(bytes[..12].try_into().unwrap());
    assert_eq!(header.ver, 2);
    assert_eq!(header.crc, witness);
    let second = writer.append_required(&value).unwrap();
    assert_eq!(second.segment(), 2);
    assert!(second.offset() > 0);
    drop(writer);

    let (mut writer, report) = open(dir.path(), options).unwrap();
    assert_eq!(report.frames, 6);
    assert_eq!(
        writer.append(&value).unwrap(),
        Lsn::from_parts(3, 0).unwrap()
    );
    assert_eq!(
        writer.append_required(&value).unwrap(),
        Lsn::from_parts(4, 0).unwrap()
    );
    drop(writer);
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let records = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(|result| result.unwrap().1.record.kind())
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        [RecordKind::Resume, RecordKind::Checkpoint].repeat(5)
    );
}

#[test]
fn required_frames_after_a_damaged_length_are_never_truncated() {
    for kind in [RecordKind::Resume.tag(), 1_000] {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 1);
        let payload = serde_json::to_vec(&entry()).unwrap();
        let mut bytes = raw_frame(1, RecordKind::Resume.tag(), &payload);
        bytes[..4].copy_from_slice(&16_777_216u32.to_le_bytes());
        bytes.extend(raw_frame(2, kind, &payload));
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Reader::open(dir.path()).unwrap_err(),
            Error::Corrupt {
                reason: FrameError::Short { .. },
                ..
            }
        ));
        assert!(matches!(
            open(dir.path(), JournalOptions::default()).unwrap_err(),
            Error::Corrupt {
                reason: FrameError::Short { .. },
                ..
            }
        ));
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn integrity_is_checked_before_required_kind_or_version_support() {
    for version in [2, 3] {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 1);
        let mut bytes = raw_frame(version, 1_000, b"future record");
        bytes[8] ^= 1;
        let payload = serde_json::to_vec(&entry()).unwrap();
        bytes.extend(raw_frame(1, RecordKind::Resume.tag(), &payload));
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Reader::open(dir.path()).unwrap_err(),
            Error::Corrupt {
                reason: FrameError::Crc { .. },
                ..
            }
        ));
        assert!(matches!(
            open(dir.path(), JournalOptions::default()).unwrap_err(),
            Error::Corrupt {
                reason: FrameError::Crc { .. },
                ..
            }
        ));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn mcp_policy_and_grant_append_in_required_v2_even_through_default_append() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let records = [
        Record::AgentMcpServersChanged {
            agent: AgentId::NIL,
            mcp_servers: Vec::new(),
        },
        Record::McpLaunchGranted {
            agent: AgentId::NIL,
            turn: dudes_types::TurnId::NIL,
            policy_lsn: Lsn(1),
            mcp_servers: Vec::new(),
        },
    ];
    let mut expected = Vec::new();
    for record in records {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        };
        let lsn = writer.append(&entry).unwrap();
        let bytes = fs::read(segment_path(dir.path(), lsn.segment())).unwrap();
        let offset = usize::try_from(lsn.offset()).unwrap();
        let header = Header::parse(
            &bytes[offset..offset + dudes_journal::frame::HEADER_LEN]
                .try_into()
                .unwrap(),
        );
        assert_eq!(header.ver, dudes_journal::frame::REQUIRED_VERSION);
        assert_eq!(header.kind, entry.record.kind().tag());
        assert!(matches!(header.kind, 69 | 70));
        expected.push((lsn, entry));
    }
    drop(writer);
    let (reader, _) = Reader::open(dir.path()).unwrap();
    assert_eq!(
        reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
}

/// A build from before tickets knows tags 1 to 70 and refuses a required frame of any other tag.
/// A ticket record written by this build is such a frame, so that build stops at the first one,
/// before it repairs the journal, replays it or opens a cache, rather than reading a ticket as a
/// plain card or folding a start as nothing.
#[test]
fn a_ticket_record_is_a_required_frame_an_older_build_refuses() {
    let before_tickets: Vec<u16> = RecordKind::KNOWN[..70]
        .iter()
        .map(|kind| kind.tag())
        .collect();
    for kind in [
        RecordKind::TicketCreated,
        RecordKind::TicketEdited,
        RecordKind::TicketAssigned,
        RecordKind::TicketStarted,
        RecordKind::TicketRemoved,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, _) = open(
            dir.path(),
            JournalOptions {
                fsync: Fsync::None,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        writer
            .append(&Entry {
                at: Timestamp::UNIX_EPOCH,
                record: dudes_proto::record::samples::record(kind),
            })
            .unwrap();
        drop(writer);
        let bytes = fs::read(segment_path(dir.path(), 1)).unwrap();
        let header = Header::parse(bytes[..12].try_into().unwrap());
        assert_eq!(header.kind, kind.tag());
        assert_eq!(header.ver, 2, "{kind:?} is written as a required frame");
        assert!(
            !before_tickets.contains(&header.kind),
            "{kind:?} is a tag the older build does not know, so it refuses the frame"
        );
        let (reader, _) = Reader::open(dir.path()).expect("this build understands it");
        assert_eq!(reader.iter_from(Lsn::ZERO).unwrap().count(), 1);
    }
}
