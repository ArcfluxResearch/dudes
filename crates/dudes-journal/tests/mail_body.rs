//! A mail body survives the journal: at or under 4 KiB in the record itself, above it in the file
//! the record names, and nowhere else. The journal is the truth about a body's existence and content.

use std::fs;
use std::path::{Path, PathBuf};

use dudes_journal::{Fsync, JournalOptions, Reader, open};
use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record};
use dudes_types::{AgentId, Lsn, MailId, MailState, Timestamp};

const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const MAIL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

/// `blake3::hash(&[b'm'; 4097]).to_hex()`, derived out of tree. The test asserts the record's
/// digest against this independent value rather than against one it computed itself.
const OVER_THRESHOLD_BLAKE3: &str =
    "505f6ab8a7d16ca7a9ba2fdef259a5236f6e12a024f24215dcfbb1ea840a48c5";

fn options() -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        ..JournalOptions::default()
    }
}

/// A mail record on one row of the presence table.
fn mail(body: Option<&str>, body_len: u64, reference: Option<(&Path, &str)>) -> Record {
    let (body_path, body_blake3) = match reference {
        Some((path, blake3)) => (Some(path.to_path_buf()), Some(blake3.to_owned())),
        None => (None, None),
    };
    Record::Mail {
        thread: None,
        thread_seq: 0,
        members: Vec::new(),
        id: MailId::parse(MAIL).unwrap(),
        from: MailFrom::Agent {
            agent: AgentId::parse(AGENT).unwrap(),
        },
        to: MailTo::Human,
        act: MailAct::Request,
        subject: "Read this".to_owned(),
        subject_admission: None,
        body: body.map(str::to_owned),
        body_len,
        body_path,
        body_blake3,
        state: MailState::Durable,
        actor: None,
        provenance: None,
    }
}

/// Append `record` to a fresh journal, then read it back through a reader opened over the files —
/// so what comes out has been through the frame, the payload encoding and the decode.
fn round_trip(record: Record) -> (tempfile::TempDir, Entry) {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("journal");
    let (mut writer, _) = open(&path, options()).unwrap();
    writer
        .append(&Entry {
            at: Timestamp::from_unix_millis(1_787_394_600_000),
            record,
        })
        .unwrap();
    drop(writer);

    let reader = Reader::new(&path);
    let entries: Vec<Entry> = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(|entry| entry.unwrap().1)
        .collect();
    let [entry] = entries.as_slice() else {
        panic!("one record was appended, so one entry must come back: {entries:?}");
    };
    (data, entry.clone())
}

/// **A 1 KiB body is recovered from the journalled record alone.**
///
/// The journal keeps content at or under 4 KiB in the record, so the decoded record is the whole of
/// what a reader needs — there is no file to consult. The control is that the data dir holds no
/// agent tree at all: without it, "recovered" could mean the body came from somewhere the record
/// does not name.
#[test]
fn a_one_kib_body_is_recovered_from_the_journalled_record_alone() {
    let text = "m".repeat(1024);
    let (data, entry) = round_trip(mail(Some(&text), 1024, None));

    let Record::Mail {
        body,
        body_len,
        body_path,
        body_blake3,
        ..
    } = &entry.record
    else {
        panic!("a mail record was appended: {entry:?}");
    };

    assert_eq!(
        body.as_deref(),
        Some(text.as_str()),
        "the 1 KiB body comes back from the record itself"
    );
    assert_eq!(
        *body_len,
        u64::try_from(text.len()).unwrap(),
        "and `body_len` is its exact UTF-8 byte length"
    );
    assert!(
        body_path.is_none() && body_blake3.is_none(),
        "a body this size names no file: {body_path:?} {body_blake3:?}"
    );
    assert!(
        !data.path().join("agents").exists(),
        "control: no body file exists, so nothing but the record could have supplied the body"
    );
}

/// **A body over the threshold is recovered from the record plus the file it names.**
///
/// The record carries the relative path and the digest; the reader resolves the path under the
/// data dir and checks what it finds against the length and the digest the record states. Nothing
/// outside the record and that one file is consulted, and the inline field stays empty on this row.
#[test]
fn an_over_threshold_body_is_recovered_from_the_record_and_its_referenced_file() {
    let bytes = vec![b'm'; 4097];
    let relative = PathBuf::from(format!("agents/{AGENT}/mail/{MAIL}.body"));
    let (data, entry) = round_trip(mail(
        None,
        u64::try_from(bytes.len()).unwrap(),
        Some((relative.as_path(), OVER_THRESHOLD_BLAKE3)),
    ));

    let absolute = data.path().join(&relative);
    fs::create_dir_all(absolute.parent().unwrap()).unwrap();
    fs::write(&absolute, &bytes).unwrap();

    let Record::Mail {
        body,
        body_len,
        body_path,
        body_blake3,
        ..
    } = &entry.record
    else {
        panic!("a mail record was appended: {entry:?}");
    };

    assert_eq!(
        *body, None,
        "a body over the threshold is not carried inline as well"
    );
    let recovered = fs::read(data.path().join(body_path.as_ref().unwrap())).unwrap();
    assert_eq!(
        recovered, bytes,
        "the body comes back from the file the record names"
    );
    assert_eq!(
        u64::try_from(recovered.len()).unwrap(),
        *body_len,
        "and its length is the one the record recorded"
    );
    assert_eq!(
        body_blake3.as_deref(),
        Some(OVER_THRESHOLD_BLAKE3),
        "against the digest of those bytes, derived out of tree"
    );
}
