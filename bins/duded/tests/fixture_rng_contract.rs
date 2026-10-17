//! The prepared fixture RNG reads the same complete prefix as the real journal reader.
mod common;

use std::any::Any;
use std::path::{Path, PathBuf};

use common::{Fixture, FixtureRng};
use dudes_journal::{Reader, frame, segment::segment_path};
use dudes_proto::{Entry, Record, RecordKind};
use dudes_supervisor::Rng;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, Lsn, Timestamp};
use ulid::Ulid;

fn entry(counter: u128) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::Resume {
            agent: AgentId::from_parts(0, counter),
            browser: None,
        },
    }
}

fn encoded(entry: &Entry) -> Vec<u8> {
    let mut bytes = Vec::new();
    frame::encode(
        entry.record.kind().tag(),
        &serde_json::to_vec(entry).unwrap(),
        &mut bytes,
    )
    .unwrap();
    bytes
}

fn write_segment(journal: &Path, number: u32, bytes: &[u8]) -> PathBuf {
    std::fs::create_dir_all(journal).unwrap();
    let path = segment_path(journal, number);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn journal_bytes(journal: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(journal)
        .unwrap()
        .map(|file| {
            let path = file.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort();
    files
}

fn panic_text(panic: &(dyn Any + Send)) -> &str {
    if let Some(text) = panic.downcast_ref::<String>() {
        text
    } else {
        panic.downcast_ref::<&str>().expect("a setup error message")
    }
}

fn assert_refused(settings: &duded::Settings, original: &str) {
    let panic = std::panic::catch_unwind(|| FixtureRng::from_rng(settings, FakeRng::new()))
        .expect_err("an existing unreadable journal cannot become fresh history");
    let message = panic_text(panic.as_ref());
    let journal = settings.state_dir.join(duded::boot::JOURNAL_DIR);
    assert!(
        message.contains(&journal.display().to_string()),
        "{message}"
    );
    assert!(message.contains(original), "{message}");
}

#[test]
fn missing_journal_preserves_fresh_draws_without_creating_files() {
    let fixture = Fixture::new();
    let mut rng = FixtureRng::from_rng(&fixture.settings(), FakeRng::new());
    assert_eq!(rng.ulid().to_string(), "00000000000000000000000001");
    assert_eq!(
        rng.agent_token().expose(),
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2"
    );
    assert!(!fixture.journal_dir().exists());
}

#[test]
fn preparation_uses_the_final_overridden_settings_path() {
    let fixture = Fixture::new();
    let alternate = tempfile::tempdir().unwrap();
    write_segment(&fixture.journal_dir(), 1, &encoded(&entry(99)));
    let mut settings = fixture.settings();
    settings.state_dir = alternate.path().to_path_buf();
    let journal = settings.state_dir.join(duded::boot::JOURNAL_DIR);
    write_segment(&journal, 1, &encoded(&entry(9)));
    let original_default = journal_bytes(&fixture.journal_dir());
    let original_alternate = journal_bytes(&journal);

    let mut rng = FixtureRng::from_rng(&settings, FakeRng::new());
    assert_eq!(rng.ulid(), Ulid::from_parts(0, 10));
    assert_eq!(journal_bytes(&fixture.journal_dir()), original_default);
    assert_eq!(journal_bytes(&journal), original_alternate);
}

#[test]
fn preparation_preserves_legitimate_predraws_and_reserves_newer_history() {
    for (recorded, next) in [(2, 5), (9, 10)] {
        let fixture = Fixture::new();
        write_segment(&fixture.journal_dir(), 1, &encoded(&entry(recorded)));
        let mut initial = FakeRng::new();
        initial.ulid();
        let consumed = initial.agent_token();
        initial.ulid();
        initial.ulid();
        let observer = initial.tokens();
        let consumed_hash = dudes_supervisor::blake3_hex(consumed.expose().as_bytes());

        let mut prepared = FixtureRng::from_rng(&fixture.settings(), initial);
        assert_eq!(prepared.ulid(), Ulid::from_parts(0, next));
        assert_eq!(
            observer.token_of(&consumed_hash).expose(),
            consumed.expose()
        );
        assert_eq!(
            prepared.tokens().token_of(&consumed_hash).expose(),
            consumed.expose()
        );
    }
}

#[test]
fn corrupt_complete_frame_preserves_the_reader_error_and_journal_bytes() {
    let fixture = Fixture::new();
    let mut corrupt = encoded(&entry(7));
    corrupt[8] ^= 1;
    write_segment(&fixture.journal_dir(), 1, &corrupt);
    write_segment(&fixture.journal_dir(), 2, &encoded(&entry(8)));
    let before = journal_bytes(&fixture.journal_dir());
    let original = Reader::open(&fixture.journal_dir()).unwrap_err();
    assert!(matches!(
        original,
        dudes_journal::Error::Corrupt {
            reason: frame::FrameError::Crc { .. },
            ..
        }
    ));

    assert_refused(&fixture.settings(), &original.to_string());
    assert_eq!(journal_bytes(&fixture.journal_dir()), before);
}

#[test]
fn malformed_json_preserves_the_decode_error_and_journal_bytes() {
    let fixture = Fixture::new();
    let mut malformed = Vec::new();
    frame::encode(RecordKind::Resume.tag(), b"{", &mut malformed).unwrap();
    write_segment(&fixture.journal_dir(), 1, &malformed);
    let before = journal_bytes(&fixture.journal_dir());
    let (reader, _) = Reader::open(&fixture.journal_dir()).unwrap();
    let original = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .next()
        .unwrap()
        .unwrap_err();
    assert!(matches!(original, dudes_journal::Error::Decode { .. }));

    assert_refused(&fixture.settings(), &original.to_string());
    assert_eq!(journal_bytes(&fixture.journal_dir()), before);
}

#[test]
fn existing_empty_journal_is_refused_without_creating_a_segment() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.journal_dir()).unwrap();
    let original = Reader::open(&fixture.journal_dir()).unwrap_err();
    assert!(matches!(original, dudes_journal::Error::NotAJournal { .. }));

    assert_refused(&fixture.settings(), &original.to_string());
    assert_eq!(journal_bytes(&fixture.journal_dir()), Vec::new());
}

#[test]
#[cfg(unix)]
fn a_file_ancestor_preserves_the_metadata_error_and_original_bytes() {
    let fixture = Fixture::new();
    let ancestor = fixture.state.path().join("file");
    std::fs::write(&ancestor, b"existing state").unwrap();
    let mut settings = fixture.settings();
    settings.state_dir = ancestor.join("state");
    let journal = settings.state_dir.join(duded::boot::JOURNAL_DIR);
    let original = std::fs::symlink_metadata(journal).unwrap_err();
    assert_ne!(original.kind(), std::io::ErrorKind::NotFound);

    assert_refused(&settings, &original.to_string());
    assert_eq!(std::fs::read(ancestor).unwrap(), b"existing state");
}

#[test]
#[cfg(unix)]
fn a_symlink_loop_preserves_the_metadata_error_and_link() {
    let fixture = Fixture::new();
    let link = fixture.state.path().join("loop");
    std::os::unix::fs::symlink("loop", &link).unwrap();
    let mut settings = fixture.settings();
    settings.state_dir = link.clone();
    let journal = settings.state_dir.join(duded::boot::JOURNAL_DIR);
    let original = std::fs::symlink_metadata(journal).unwrap_err();
    assert_ne!(original.kind(), std::io::ErrorKind::NotFound);

    assert_refused(&settings, &original.to_string());
    assert_eq!(std::fs::read_link(link).unwrap(), Path::new("loop"));
}

#[test]
fn preparation_matches_the_readers_complete_prefix_and_leaves_a_torn_tail() {
    let fixture = Fixture::new();
    let complete = entry(7);
    let mut bytes = encoded(&complete);
    let end = Lsn::from_parts(1, u64::try_from(bytes.len()).unwrap()).unwrap();
    let unfinished = encoded(&entry(99));
    bytes.extend_from_slice(&unfinished[..=frame::HEADER_LEN]);
    write_segment(&fixture.journal_dir(), 1, &bytes);
    let before = journal_bytes(&fixture.journal_dir());
    let (reader, report) = Reader::open(&fixture.journal_dir()).unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.end, end);
    assert_eq!(
        report.torn,
        Some(dudes_journal::reader::Torn {
            lsn: end,
            dropped_bytes: 13,
        })
    );
    assert_eq!(
        reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(|item| item.unwrap().1)
            .collect::<Vec<_>>(),
        vec![complete]
    );

    let mut rng = FixtureRng::from_rng(&fixture.settings(), FakeRng::new());
    assert_eq!(rng.ulid(), Ulid::from_parts(0, 8));
    assert_eq!(journal_bytes(&fixture.journal_dir()), before);
    assert_eq!(Reader::open(&fixture.journal_dir()).unwrap().1, report);
}

fn rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

fn unprepared_rng_fields(source: &str) -> Vec<(String, usize, String)> {
    let mut function = "";
    let mut depth = 0_i32;
    let mut body = false;
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap().trim();
        if let Some(signature) = code.strip_prefix("fn ") {
            function = signature.split('(').next().unwrap();
            depth = 0;
            body = false;
        }
        if !function.is_empty() {
            body |= code.contains('{');
            depth += i32::try_from(code.matches('{').count()).unwrap();
            depth -= i32::try_from(code.matches('}').count()).unwrap();
            if body {
                if let Some((_, value)) = code.split_once("rng:") {
                    let value = value.trim();
                    if !value.starts_with("common::FixtureRng::from_rng(")
                        && !value.starts_with("FixtureRng::from_rng(")
                        && !value.starts_with("parts.rng,")
                    {
                        found.push((function.to_owned(), index + 1, value.to_owned()));
                    }
                } else if code == "rng," {
                    found.push((function.to_owned(), index + 1, "rng,".to_owned()));
                }
                if depth == 0 {
                    function = "";
                }
            }
        }
    }
    found
}

#[test]
fn the_function_name_guard_rejects_a_planted_raw_rng_field() {
    let planted = "fn ordinary_fixture() {\n    Seams {\n        rng: FakeRng::new(),\n    }\n}\n";
    assert_eq!(
        unprepared_rng_fields(planted),
        vec![(
            "ordinary_fixture".to_owned(),
            3,
            "FakeRng::new(),".to_owned()
        )]
    );
    let inline = "fn ordinary_fixture() { Seams { rng: FakeRng::new(), ..parts } }";
    assert_eq!(
        unprepared_rng_fields(inline),
        vec![(
            "ordinary_fixture".to_owned(),
            1,
            "FakeRng::new(), ..parts } }".to_owned()
        )]
    );
    for prepared in [
        "fn ordinary_fixture() { Seams { rng: parts.rng, ..rest } }",
        "fn ordinary_fixture() { Seams { rng: FixtureRng::from_rng(settings, initial), ..rest } }",
    ] {
        assert!(unprepared_rng_fields(prepared).is_empty());
    }
}

#[test]
fn daemon_and_cli_fixture_rng_fields_have_only_the_named_boundary_exceptions() {
    let bins = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut sources = Vec::new();
    rust_sources(&bins.join("duded/tests"), &mut sources);
    rust_sources(&bins.join("dudes/tests"), &mut sources);
    let mut raw = Vec::new();
    let mut native_random_fields = 0;
    for path in sources {
        if path.file_name().unwrap() == "fixture_rng_contract.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (function, line, value) in unprepared_rng_fields(&source) {
            let relative = path
                .strip_prefix(bins)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if relative == "duded/tests/chat_browser_fixture.rs"
                && function == "interactive_conversation_fixture"
                && value == "FixtureRng,"
            {
                native_random_fields += 1;
                continue;
            }
            raw.push((relative, function, line));
        }
    }
    assert_eq!(native_random_fields, 1);
    raw.sort();
    let actual: Vec<_> = raw
        .iter()
        .map(|(path, function, _)| (path.as_str(), function.as_str()))
        .collect();
    assert_eq!(
        actual,
        vec![
            (
                "duded/tests/chat_record_prefixes.rs",
                "daemon_refuses_framing"
            ),
            (
                "duded/tests/floor_actions.rs",
                "raw_mixed_goal_attribution_refusal_seams"
            ),
            (
                "duded/tests/goal_replace.rs",
                "invalid_record_cannot_bypass_startup_validation_through_cached_projections"
            ),
            ("duded/tests/recovery.rs", "backend"),
            ("duded/tests/recovery.rs", "snapshot"),
            ("duded/tests/validation_scratch.rs", "daemon_scratch_child"),
        ],
        "RNG fields are checked within named source functions, not by a Rust parser: {raw:?}"
    );
}
