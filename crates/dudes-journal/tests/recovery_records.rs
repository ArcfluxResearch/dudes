//! Required recovery envelopes are recognized from their original bytes before projection writes.

use std::fs;

use dudes_journal::frame::{Header, crc32c};
use dudes_journal::segment::segment_path;
use dudes_journal::{Error, Reader};
use dudes_proto::record::samples;
use dudes_proto::{Entry, RecordKind};
use dudes_types::{Lsn, Timestamp};

const KINDS: [RecordKind; 3] = [
    RecordKind::RecoveryRequired,
    RecordKind::RecoveryAuthorized,
    RecordKind::RecoveryInvalidated,
];

fn raw(version: u16, kind: u16, payload: &[u8]) -> Vec<u8> {
    let header = Header {
        ver: version,
        kind,
        len: u32::try_from(payload.len()).unwrap(),
        crc: crc32c(version, kind, payload),
    };
    let mut bytes = header.to_bytes().to_vec();
    bytes.extend(payload);
    bytes
}

fn payload(kind: RecordKind) -> String {
    serde_json::to_string(&Entry {
        at: Timestamp::UNIX_EPOCH,
        record: samples::record(kind),
    })
    .unwrap()
}

#[test]
fn recovery_payloads_require_their_exact_required_header() {
    for kind in KINDS {
        for (version, tag) in [(1, kind.tag()), (2, RecordKind::Pause.tag()), (1, 1000)] {
            let dir = tempfile::tempdir().unwrap();
            let bytes = raw(version, tag, payload(kind).as_bytes());
            let path = segment_path(dir.path(), 1);
            fs::write(&path, &bytes).unwrap();
            let error = Reader::open(dir.path()).unwrap_err();
            assert!(
                matches!(error, Error::RecoveryFrame { lsn, kind: actual, version: ver, expected_kind }
                if lsn == Lsn::from_parts(1, 0).unwrap() && actual == tag && ver == version && expected_kind == kind.tag()),
                "{error}"
            );
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn duplicate_recovery_discriminators_under_unknown_compatible_headers_refuse() {
    for kind in KINDS {
        let encoded = payload(kind);
        for conflict in ["null", "1e9999", "{}", "\"other\""] {
            for original in [
                encoded.replacen('{', &format!("{{\"t\":{conflict},"), 1),
                format!("{},\"t\":{conflict}}}", encoded.strip_suffix('}').unwrap()),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = segment_path(dir.path(), 1);
                let bytes = raw(1, 1000, original.as_bytes());
                fs::write(&path, &bytes).unwrap();
                assert!(
                    matches!(Reader::open(dir.path()), Err(Error::Decode { lsn, .. }) if lsn == Lsn::from_parts(1, 0).unwrap())
                );
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }
}

#[test]
fn recovery_headers_cannot_carry_another_payload_or_unknown_required_kind() {
    let ordinary = payload(RecordKind::Pause);
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 1);
        let bytes = raw(2, kind.tag(), ordinary.as_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Reader::open(dir.path()), Err(Error::RecoveryFrame { expected_kind, .. }) if expected_kind == RecordKind::Pause.tag())
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        segment_path(dir.path(), 1),
        raw(2, 1000, payload(KINDS[0]).as_bytes()),
    )
    .unwrap();
    assert!(matches!(
        Reader::open(dir.path()),
        Err(Error::RequiredKind { kind: 1000, .. })
    ));
}

#[test]
fn recovery_original_json_rejects_unknown_omitted_and_malformed_authority() {
    for kind in KINDS {
        let encoded = payload(kind);
        for body in [
            encoded.replacen('{', "{\"unknown\":true,", 1),
            format!("{encoded} trailing"),
            encoded.replacen("\"binding\"", "\"other_binding\"", 1),
        ] {
            if kind == RecordKind::RecoveryInvalidated && body == encoded {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = segment_path(dir.path(), 1);
            let bytes = raw(2, kind.tag(), body.as_bytes());
            fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                Reader::open(dir.path()),
                Err(Error::Decode { .. })
            ));
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn unrelated_unknown_compatible_bytes_keep_their_skip_contract() {
    for body in [
        b"arbitrary compatible bytes".as_slice(),
        br#"{"nested":{"t":"recovery_required"}}"#,
        br#"{"broken":?,"t":"recovery_required"}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(segment_path(dir.path(), 1), raw(1, 1000, body)).unwrap();
        let (reader, report) = Reader::open(dir.path()).unwrap();
        assert_eq!(report.frames, 1);
        assert_eq!(reader.iter_from(Lsn::ZERO).unwrap().count(), 0);
    }
}
