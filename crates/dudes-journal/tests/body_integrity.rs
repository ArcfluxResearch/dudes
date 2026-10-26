//! `system.doctor` body-integrity collection at the journal reader seam.

use std::fs;
use std::path::{Path, PathBuf};

use dudes_journal::{Fsync, JournalOptions, Reader, body_integrity, open};
use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record};
use dudes_types::{AgentId, Lsn, MailId, MailState, Timestamp};

/// The suite's control body occupies the **legal out-of-line row**: over
/// 4,096 bytes, which is the only row on which a reference belongs. A short body carrying both
/// reference fields would be corruption, and the suite must not assert such a row clean.
const BODY: &[u8] = &[b'm'; 4097];

/// `blake3::hash(&[b'm'; 4097]).to_hex()`, derived out of tree so the digest is an independent
/// value rather than one the code under test supplies.
const BODY_BLAKE3: &str = "505f6ab8a7d16ca7a9ba2fdef259a5236f6e12a024f24215dcfbb1ea840a48c5";

const _: () = assert!(
    BODY.len() > 4096,
    "the control body must sit above the inline threshold, or it occupies a corrupt row"
);

const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const LEGAL_MAIL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const PLANTED_MAIL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const SECOND_PLANTED_MAIL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";

fn options() -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        ..JournalOptions::default()
    }
}

/// A body path as the mailbox writes them: relative to the data dir, never absolute.
fn relative(mail: &str) -> PathBuf {
    PathBuf::from(format!("agents/{AGENT}/mail/{mail}.body"))
}

/// One mail record. `body` and `reference` are the two halves the presence
/// table keys on; every other field is fixed, so a failure names the row and not the fixture.
fn mail(id: &str, body: Option<&str>, body_len: u64, reference: Option<&Path>) -> Record {
    Record::Mail {
        thread: None,
        thread_seq: 0,
        members: Vec::new(),
        id: MailId::parse(id).unwrap(),
        from: MailFrom::Agent {
            agent: AgentId::parse(AGENT).unwrap(),
        },
        to: MailTo::Human,
        act: MailAct::Request,
        subject: "Read this".to_owned(),
        subject_admission: None,
        body: body.map(str::to_owned),
        body_len,
        body_path: reference.map(Path::to_path_buf),
        body_blake3: reference.map(|_| BODY_BLAKE3.to_owned()),
        state: MailState::Durable,
        actor: None,
        provenance: None,
    }
}

/// A journal holding `records`, in a fresh data dir.
fn journal(records: Vec<Record>) -> (tempfile::TempDir, Reader) {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("journal");
    let (mut writer, _) = open(&path, options()).unwrap();
    for record in records {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_787_394_600_000),
                record,
            })
            .unwrap();
    }
    drop(writer);

    let reader = Reader::new(&path);
    (data, reader)
}

fn write_body(data_dir: &Path, relative: &Path, bytes: &[u8]) {
    let body = data_dir.join(relative);
    fs::create_dir_all(body.parent().unwrap()).unwrap();
    fs::write(&body, bytes).unwrap();
}

/// One legal out-of-line record whose referenced body is on disk and intact.
fn fixture() -> (tempfile::TempDir, Reader, PathBuf) {
    let path = relative(LEGAL_MAIL);
    let (data, reader) = journal(vec![mail(
        LEGAL_MAIL,
        None,
        u64::try_from(BODY.len()).unwrap(),
        Some(&path),
    )]);
    write_body(data.path(), &path, BODY);
    (data, reader, path)
}

fn paths(rows: &[dudes_proto::rpc::system::BodyIntegrity]) -> Vec<&Path> {
    rows.iter().map(|row| row.path.as_path()).collect()
}

#[test]
fn deleted_referenced_body_is_reported_and_intact_control_is_clean() {
    let (data, reader, relative) = fixture();
    assert!(
        body_integrity(&reader, data.path()).unwrap().is_empty(),
        "the intact-body control has no diagnostic rows"
    );

    fs::remove_file(data.path().join(&relative)).unwrap();
    let rows = body_integrity(&reader, data.path()).unwrap();
    assert_eq!(paths(&rows), vec![relative.as_path()]);
    assert_eq!(
        rows[0].problem,
        dudes_proto::rpc::system::BodyIntegrityProblem::Missing
    );

    assert_eq!(reader.iter_from(Lsn::ZERO).unwrap().count(), 1);
}

#[test]
fn body_truncated_below_its_recorded_decoded_length_is_reported() {
    let (data, reader, relative) = fixture();
    fs::write(data.path().join(&relative), &BODY[..BODY.len() - 1]).unwrap();

    let rows = body_integrity(&reader, data.path()).unwrap();
    assert_eq!(paths(&rows), vec![relative.as_path()]);
    assert_eq!(
        rows[0].problem,
        dudes_proto::rpc::system::BodyIntegrityProblem::Truncated
    );
}

#[test]
fn same_length_body_with_a_different_hash_is_reported() {
    let (data, reader, relative) = fixture();
    let mut changed = BODY.to_vec();
    *changed.last_mut().unwrap() = b'z';
    assert_eq!(changed.len(), BODY.len(), "the mutation preserves length");
    fs::write(data.path().join(&relative), changed).unwrap();

    let rows = body_integrity(&reader, data.path()).unwrap();
    assert_eq!(paths(&rows), vec![relative.as_path()]);
    assert_eq!(
        rows[0].problem,
        dudes_proto::rpc::system::BodyIntegrityProblem::HashMismatch
    );
}

/// Reader disposition: a record matching no row of the presence table
/// **yields no body to any consumer** — this collector prefers neither half and never resolves the
/// reference. This case is inline content beside a reference
/// whose file is absent.
///
/// **The control is in the same journal and in the same state.** The legal out-of-line record's
/// file is absent too, so the two assertions below say both halves at once: the legal row is
/// reported exactly once, and the corrupt row is not reported at all. A collector that preferred
/// the reference would return two rows.
///
/// The corrupt record is planted **above** the threshold deliberately. At or under 4,096 bytes the
/// sub-threshold rule would suppress it as well and this case could not tell the two halves of the
/// disposition apart; here only `body: None` in the collector's pattern can.
#[test]
fn inline_content_beside_a_reference_yields_no_row_against_a_missing_control() {
    let legal = relative(LEGAL_MAIL);
    let planted = relative(PLANTED_MAIL);
    let inline = "m".repeat(4097);
    let (data, reader) = journal(vec![
        mail(LEGAL_MAIL, None, 4097, Some(&legal)),
        mail(PLANTED_MAIL, Some(&inline), 4097, Some(&planted)),
    ]);

    let rows = body_integrity(&reader, data.path()).unwrap();

    assert_eq!(
        paths(&rows),
        vec![legal.as_path()],
        "a record carrying inline content beside a reference yields no row: {rows:?}"
    );
    assert_eq!(
        rows[0].problem,
        dudes_proto::rpc::system::BodyIntegrityProblem::Missing,
        "control: the legal out-of-line record in the same state is still reported"
    );
}

/// **a reference on a record whose `body_len` is at or under 4,096 is
/// corrupt**, not a retained legacy shape — the threshold is conditional,
/// so a body that small belongs in the
/// record. The disposition for this case is
/// no row at all.
///
/// **Both operands are planted**, because one of them proves only half the rule. The first subject
/// sits at exactly 4,096, so a collector written with `<` in place of `<=` resolves it and fails
/// here; the second sits far below at 21 bytes, so
/// an `==` in the same place fails here too. The control is the legal row one byte above the
/// threshold, in the same journal and the same state, which a `>=` would lose.
#[test]
fn a_reference_at_or_under_the_threshold_yields_no_row_against_a_missing_control() {
    let legal = relative(LEGAL_MAIL);
    let planted = relative(PLANTED_MAIL);
    let well_under = relative(SECOND_PLANTED_MAIL);
    let (data, reader) = journal(vec![
        mail(LEGAL_MAIL, None, 4097, Some(&legal)),
        mail(PLANTED_MAIL, None, 4096, Some(&planted)),
        mail(SECOND_PLANTED_MAIL, None, 21, Some(&well_under)),
    ]);

    let rows = body_integrity(&reader, data.path()).unwrap();

    assert_eq!(
        paths(&rows),
        vec![legal.as_path()],
        "a reference at or under the threshold yields no row: {rows:?}"
    );
    assert_eq!(
        rows[0].problem,
        dudes_proto::rpc::system::BodyIntegrityProblem::Missing,
        "control: the legal out-of-line record one byte above the threshold is still reported"
    );
}
