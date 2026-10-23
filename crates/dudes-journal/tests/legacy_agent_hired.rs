//! trust is declared at the hire, recorded there, and **absence means
//! untrusted**: a journal written before this record replays as untrusted rather than as
//! trusted, because the safe reading is the one that survives replay (invariant 1).
//!
//! The property is about **absence**, so the fixture is bytes rather than a `Record` built with
//! the field left out: the payload below is the literal `agent_hired` JSON a build before the
//! field wrote, framed the way [`dudes_journal::frame::encode`] frames it, laid down as a
//! segment, and read back through the same [`Reader`] an offline `dudes replay` uses. A test
//! that round-tripped a `Record` through serde would prove only that a defaulted field decodes
//! from a value serde itself had just produced, and would stay green over a fixture that
//! quietly carried `"trusted": false`.
//!
//! **Every arm here has a control that can see its assertion fail.** The subject asserts an
//! absent key replays as untrusted; the control asserts the byte-identical fixture *plus*
//! `"trusted": true` replays as trusted. Without the control, a `trusted` hard-wired to `false`
//! — or never read at all — passes the subject. With it, the pair is a claim about what absence
//! means rather than about a constant. The third test carries the same pair through the real
//! writer, so the field is known to survive an append and not only a hand-built frame.

use std::fs;
use std::path::{Path, PathBuf};

use dudes_journal::segment::segment_path;
use dudes_journal::{FIRST_SEGMENT, Fsync, JournalOptions, Reader, frame, open};
use dudes_proto::{AgentBudget, Entry, Record, RecordKind};
use dudes_types::{AgentId, Lsn, Provider, Timestamp, Tokens, Usd};

/// The `agent_hired` payload as a build before this field wrote it: every key `AgentHired`
/// carried before the field was added, minus the two optional ones. `sandbox` is absent as well as `trusted`,
/// which is what a journal from before the repository-trust field actually looks like.
const LEGACY: &str = r#"{"at":1787394600000,"t":"agent_hired","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","name":"Kim","role":"backend engineer","department":"engineering","provider":"claude","cwd":"/work/repo","budget":{"usd_per_day":20000000,"tokens_per_day":2000000}}"#;

/// Lay `payload` down as the whole of the first segment and read the journal back. Nothing here
/// goes through the writer: the point is bytes this build did not produce.
fn replay_payload(dir: &Path, payload: &str) -> Vec<Entry> {
    let mut bytes = Vec::new();
    frame::encode(RecordKind::AgentHired.tag(), payload.as_bytes(), &mut bytes).unwrap();
    fs::write(segment_path(dir, FIRST_SEGMENT), &bytes).unwrap();
    Reader::new(dir)
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(|item| item.unwrap().1)
        .collect()
}

/// The same fixture with one key added, so the two arms differ in exactly the thing under test.
fn with_trusted(value: bool) -> String {
    let (head, tail) = LEGACY.split_at(1);
    format!("{head}\"trusted\":{value},{tail}")
}

/// **Subject.** A journal written before the field replays as untrusted.
#[test]
fn a_journal_written_before_the_field_replays_as_untrusted() {
    assert!(
        !LEGACY.contains("trusted"),
        "the fixture must carry no trust key at all: this test is about absence, and a fixture \
         with `\"trusted\": false` in it would assert nothing"
    );
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), LEGACY);
    assert_eq!(entries.len(), 1, "the fixture is one whole frame");
    match &entries[0].record {
        Record::AgentHired {
            trusted,
            browser_onboarding: None,
            sandbox,
            name,
            cwd,
            ..
        } => {
            assert!(
                !*trusted,
                "absence means untrusted, so a pre-field journal replays untrusted"
            );
            assert_eq!(
                *sandbox, None,
                "the other absent optional field is unchanged"
            );
            assert_eq!(name, "Kim", "the rest of the record decoded");
            assert_eq!(
                cwd,
                Path::new("/work/repo"),
                "the rest of the record decoded"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_legacy_hire_has_no_recorded_workspace_even_when_cwd_is_present() {
    assert!(!LEGACY.contains("workspace"));
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), LEGACY);
    match &entries[0].record {
        Record::AgentHired { workspace, .. } => assert_eq!(*workspace, None),
        other => panic!("unexpected {other:?}"),
    }
    let mut projections = dudes_journal::Projections::in_memory().unwrap();
    projections.apply(Lsn(1), 1, &entries[0]).unwrap();
    assert_eq!(projections.agents().unwrap()[0].workspace, None);
}

/// **Control for the subject.** The byte-identical fixture plus `"trusted": true` replays as
/// trusted. This is what makes the subject a claim about absence: a `trusted` that is hard-wired
/// to `false`, or dropped on the read path, fails here while the subject stays green.
#[test]
fn an_explicit_trust_declaration_replays_as_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), &with_trusted(true));
    match &entries[0].record {
        Record::AgentHired { trusted, .. } => assert!(
            *trusted,
            "a recorded trust declaration is what replay reads; only the recorded value decides \
             a later turn"
        ),
        other => panic!("unexpected {other:?}"),
    }

    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), &with_trusted(false));
    match &entries[0].record {
        Record::AgentHired { trusted, .. } => {
            assert!(!*trusted, "an explicit false is untrusted, as absence is");
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// The field survives the write path, not only a hand-built frame: both polarities appended
/// through the one writer and read back through the reader.
#[test]
fn trust_survives_an_append() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    for trusted in [true, false] {
        writer.append(&hired(trusted)).unwrap();
    }
    let entries: Vec<Entry> = Reader::new(dir.path())
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(|item| item.unwrap().1)
        .collect();
    let read: Vec<bool> = entries
        .iter()
        .map(|entry| match &entry.record {
            Record::AgentHired { trusted, .. } => *trusted,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(read, vec![true, false], "what was appended is what replays");
}

fn hired(trusted: bool) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::AgentHired {
            character: None,
            accent: None,
            id: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            name: "Kim".into(),
            role: "backend engineer".into(),
            department: "engineering".into(),
            provider: Provider::Claude,
            cwd: PathBuf::from("/work/repo"),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(20)),
                tokens_per_day: Some(Tokens::new(2_000_000)),
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: None,
            trusted,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        },
    }
}

/// **Subject.** A journal written before a ceiling became optional folds unchanged: the recorded
/// dollar figure replays as a ceiling, not as absence.
///
/// [`LEGACY`] is exactly that journal — its `budget` object carries `"usd_per_day": 20000000`,
/// written by a build for which the field could not be absent. Reading it through the same
/// [`Reader`] an offline replay uses is what makes this a claim about bytes this build did not
/// produce; a `Record` round-tripped through serde would prove only that a value serde had just
/// written decodes again.
#[test]
fn a_journal_written_before_optional_ceilings_folds_unchanged() {
    assert!(
        LEGACY.contains(r#""usd_per_day":20000000"#),
        "the fixture must carry the pre-change spelling: this test is about a value written when \
         the field was not an Option, and a fixture without it would assert nothing"
    );
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), LEGACY);
    match &entries[0].record {
        Record::AgentHired { budget, .. } => {
            assert_eq!(
                budget.usd_per_day,
                Some(Usd::from_dollars(20)),
                "a recorded ceiling folds into Some, so nothing an operator set is lost"
            );
            assert_eq!(budget.tokens_per_day, Some(Tokens::new(2_000_000)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// **Control for the subject.** The byte-identical fixture with the dollar key *removed* folds to
/// absence — no ceiling, and in particular not `Usd::ZERO`.
///
/// Without this arm a `usd_per_day` hard-wired to `Some(Usd::from_dollars(20))`, or one defaulted
/// to `Some(Usd::ZERO)`, would satisfy the subject. With it, the pair is a claim about what a
/// present value and an absent key each mean, rather than about a constant.
#[test]
fn a_journal_with_no_dollar_ceiling_folds_to_absence() {
    let without = LEGACY.replace(r#""usd_per_day":20000000,"#, "");
    assert!(
        !without.contains("usd_per_day"),
        "the control differs from the subject in exactly the key under test"
    );
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), &without);
    match &entries[0].record {
        Record::AgentHired { budget, .. } => {
            assert_eq!(
                budget.usd_per_day, None,
                "an absent key is no ceiling; Usd::ZERO would be a ceiling of nothing, which \
                 refuses every turn — the opposite instruction"
            );
            assert_eq!(
                budget.tokens_per_day,
                Some(Tokens::new(2_000_000)),
                "and the sibling key is untouched, so the removal was surgical"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn old_hire_bytes_replay_with_automatic_appearance() {
    let dir = tempfile::tempdir().unwrap();
    let entries = replay_payload(dir.path(), LEGACY);
    let Record::AgentHired {
        character, accent, ..
    } = &entries[0].record
    else {
        panic!("hire")
    };
    assert_eq!(*character, None);
    assert_eq!(*accent, None);
    let encoded = serde_json::to_value(&entries[0]).unwrap();
    assert!(encoded.get("character").is_none());
    assert!(encoded.get("accent").is_none());
}
