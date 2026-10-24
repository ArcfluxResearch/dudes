//! A viewer's bounded reads of memory: pages newest first, one entry, the summary and the withheld
//! appends, each labelled with its entry's time and recorded writer.

use std::borrow::Cow;

use dudes_journal::{MemoryApply, MemoryProjection};
use dudes_proto::{Entry, PayloadDigest, PeerProvenance, Record};
use dudes_types::{AgentId, Lsn, Timestamp};

const BASE: i64 = 1_787_394_600_000;

fn keep(text: &str) -> Cow<'_, str> {
    Cow::Borrowed(text)
}

fn agent() -> AgentId {
    AgentId::parse("01J00000000000000000000000").unwrap()
}

fn other() -> AgentId {
    AgentId::parse("01J00000000000000000000001").unwrap()
}

fn at(lsn: u64) -> Timestamp {
    Timestamp::from_unix_millis(BASE + i64::try_from(lsn).unwrap())
}

fn fold(memory: &mut MemoryProjection, lsn: u64, record: Record) -> MemoryApply {
    memory
        .apply_entry(
            Lsn(lsn),
            &Entry {
                at: at(lsn),
                record,
            },
        )
        .unwrap()
}

fn append(who: AgentId, text: &str, provenance: Option<PeerProvenance>) -> Record {
    Record::MemoryAppend {
        agent: who,
        text: text.to_owned(),
        body: None,
        provenance,
    }
}

fn withheld(who: AgentId, len: u64, provenance: PeerProvenance, reason: &str) -> Record {
    Record::MemoryAppendQuarantined {
        agent: who,
        payload: PayloadDigest {
            decoded_len: len,
            blake3: "0".repeat(64),
        },
        provenance,
        reason: reason.to_owned(),
    }
}

fn projection() -> (tempfile::TempDir, MemoryProjection) {
    let root = tempfile::tempdir().unwrap();
    let memory = MemoryProjection::new(root.path()).unwrap();
    (root, memory)
}

#[test]
fn a_page_is_newest_first_below_its_cursor_with_each_entrys_time_and_writer() {
    let (_root, mut memory) = projection();
    let own = Some(PeerProvenance::Agent(agent()));
    for lsn in 1..=5 {
        assert_eq!(
            fold(
                &mut memory,
                lsn * 10,
                append(agent(), &format!("note {lsn}"), own)
            ),
            MemoryApply::Applied
        );
    }
    fold(
        &mut memory,
        60,
        append(
            agent(),
            "from the operator",
            Some(PeerProvenance::NoRecordedAgent),
        ),
    );
    fold(&mut memory, 70, append(agent(), "legacy", None));
    fold(
        &mut memory,
        75,
        append(
            other(),
            "another agent's",
            Some(PeerProvenance::Agent(other())),
        ),
    );

    let first = memory.page(agent(), None, 3, 2_048, &keep);
    assert_eq!(first.count, 7);
    assert_eq!(first.through, None);
    assert!(first.more);
    let seen: Vec<_> = first
        .entries
        .iter()
        .map(|entry| {
            (
                entry.lsn,
                entry.at,
                entry.provenance,
                entry.text.as_str(),
                entry.condensed,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (Lsn(70), at(70), None, "legacy", false),
            (
                Lsn(60),
                at(60),
                Some(PeerProvenance::NoRecordedAgent),
                "from the operator",
                false
            ),
            (Lsn(50), at(50), own, "note 5", false),
        ]
    );

    let second = memory.page(agent(), Some(Lsn(50)), 3, 2_048, &keep);
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| entry.lsn)
            .collect::<Vec<_>>(),
        [Lsn(40), Lsn(30), Lsn(20)],
        "the cursor is exclusive"
    );
    assert!(second.more);
    let last = memory.page(agent(), Some(Lsn(20)), 3, 2_048, &keep);
    assert_eq!(
        last.entries
            .iter()
            .map(|entry| entry.lsn)
            .collect::<Vec<_>>(),
        [Lsn(10)]
    );
    assert!(!last.more);

    let empty = memory.page(other(), Some(Lsn(75)), 3, 2_048, &keep);
    assert_eq!(
        (empty.count, empty.entries.len(), empty.more),
        (1, 0, false)
    );
    let none = memory.page(AgentId::NIL, None, 3, 2_048, &keep);
    assert_eq!((none.count, none.entries.len(), none.more), (0, 0, false));
}

#[test]
fn a_record_folded_without_its_entry_is_labelled_with_the_epoch() {
    let (_root, mut memory) = projection();
    memory.apply(Lsn(3), &append(agent(), "no time", None));
    assert_eq!(
        memory.page(agent(), None, 1, 2_048, &keep).entries[0].at,
        Timestamp::UNIX_EPOCH
    );
}

#[test]
fn previews_and_whole_entries_are_cut_at_a_character_boundary_with_the_whole_length() {
    let (_root, mut memory) = projection();
    let text = format!("{}🦀tail", "a".repeat(2_046));
    fold(&mut memory, 1, append(agent(), &text, None));

    let preview = &memory.page(agent(), None, 1, 2_048, &keep).entries[0];
    assert_eq!(preview.text, "a".repeat(2_046));
    assert!(preview.cut);
    assert_eq!(preview.len, 2_054);

    let whole = memory.entry(agent(), Lsn(1), 65_536, &keep).unwrap();
    assert_eq!(whole.text, text);
    assert!(!whole.cut);
    assert_eq!(whole.len, 2_054);
}

#[test]
fn a_secret_across_a_cut_is_scrubbed_before_the_cut_and_never_half_shown() {
    const SECRET: &str = "tok_9fQ2LmZx81KdPvR4sWbN7hYcA3eJuT6o";
    fn hide(text: &str) -> Cow<'_, str> {
        Cow::Owned(text.replace(SECRET, "(redacted)"))
    }
    let secret = SECRET;
    let (_root, mut memory) = projection();
    let text = format!("{}{secret}{}", "a".repeat(2_030), "b".repeat(1_100));
    fold(&mut memory, 1, append(agent(), &text, None));

    let preview = &memory.page(agent(), None, 1, 2_048, &hide).entries[0];
    assert_eq!(
        preview.text,
        format!("{}(redacted){}", "a".repeat(2_030), "b".repeat(8))
    );
    assert!(preview.cut, "the original was longer than the preview");
    assert_eq!(preview.len, u64::try_from(text.len()).unwrap());

    let whole = memory.entry(agent(), Lsn(1), 65_536, &hide).unwrap();
    assert!(!whole.text.contains("tok_9f"));
    assert!(!whole.cut);
}

#[test]
fn an_entry_that_is_short_once_scrubbed_is_not_reported_as_cut() {
    const SECRET: &str = "tok_9fQ2LmZx81KdPvR4sWbN7hYcA3eJuT6o";
    fn hide(text: &str) -> Cow<'_, str> {
        Cow::Owned(text.replace(SECRET, "(redacted)"))
    }
    let (_root, mut memory) = projection();
    let text = SECRET.repeat(60);
    assert!(text.len() > 2_048);
    fold(&mut memory, 1, append(agent(), &text, None));
    let preview = &memory.page(agent(), None, 1, 2_048, &hide).entries[0];
    assert_eq!(preview.text, "(redacted)".repeat(60));
    assert!(!preview.cut, "everything the entry holds was shown");
}

#[test]
fn an_entry_is_read_only_at_an_applied_position_of_that_agent() {
    let (_root, mut memory) = projection();
    fold(&mut memory, 1, append(agent(), "mine", None));
    fold(&mut memory, 2, append(other(), "theirs", None));
    fold(
        &mut memory,
        3,
        withheld(agent(), 9, PeerProvenance::NoRecordedAgent, "refused"),
    );
    assert_eq!(
        memory.entry(agent(), Lsn(1), 10, &keep).unwrap().text,
        "mine"
    );
    assert_eq!(
        memory.entry(agent(), Lsn(2), 10, &keep),
        None,
        "another agent's entry"
    );
    assert_eq!(
        memory.entry(agent(), Lsn(3), 10, &keep),
        None,
        "a withheld append"
    );
    assert_eq!(memory.entry(agent(), Lsn(4), 10, &keep), None, "no record");
}

#[test]
fn withheld_appends_are_counted_with_their_newest_eight_and_reach_no_other_read() {
    let (_root, mut memory) = projection();
    fold(&mut memory, 1, append(agent(), "kept", None));
    for lsn in 2..=11 {
        assert_eq!(
            fold(
                &mut memory,
                lsn,
                withheld(
                    agent(),
                    lsn * 100,
                    PeerProvenance::Agent(agent()),
                    "too long"
                ),
            ),
            MemoryApply::Ignored
        );
    }
    fold(
        &mut memory,
        12,
        withheld(agent(), 7, PeerProvenance::NoRecordedAgent, "no authority"),
    );
    assert_eq!(
        fold(
            &mut memory,
            12,
            withheld(agent(), 7, PeerProvenance::NoRecordedAgent, "no authority"),
        ),
        MemoryApply::Ignored
    );

    let seen = memory.withheld(agent());
    assert_eq!(
        seen.count, 11,
        "a refold of the same position is not counted again"
    );
    assert_eq!(seen.newest.len(), 8);
    let newest = &seen.newest[0];
    assert_eq!(
        (
            newest.lsn,
            newest.at,
            newest.len,
            newest.provenance,
            newest.reason.as_str()
        ),
        (
            Lsn(12),
            at(12),
            7,
            PeerProvenance::NoRecordedAgent,
            "no authority"
        )
    );
    assert_eq!(
        seen.newest
            .iter()
            .map(|marker| marker.lsn)
            .collect::<Vec<_>>(),
        [12, 11, 10, 9, 8, 7, 6, 5].map(Lsn)
    );
    assert_eq!(seen.newest[1].len, 1_100);

    assert_eq!(memory.withheld(other()).count, 0);
    assert_eq!(memory.digest(agent()), "kept");
    assert_eq!(
        memory.read(agent(), None, 1_000).unwrap(),
        ("kept".to_owned(), Lsn(1))
    );
    assert_eq!(memory.page(agent(), None, 32, 2_048, &keep).count, 1);
}

#[cfg(unix)]
#[test]
fn the_summary_names_its_kind_coverage_and_time_and_marks_the_entries_it_covers() {
    use dudes_journal::{MemoryCondenseConfig, MemorySummaryKind};

    let (_root, mut memory) = projection();
    for lsn in 1..=20 {
        fold(
            &mut memory,
            lsn,
            append(
                agent(),
                &format!("decision {lsn}: {}", "detail ".repeat(100)),
                None,
            ),
        );
    }
    assert!(memory.summary(agent()).is_none());
    let plan = memory
        .plan_condense(agent(), MemoryCondenseConfig::new(1, 4).unwrap())
        .unwrap()
        .expect("past the threshold with room to evict");
    let prepared = memory.prepare_condense(plan).unwrap();
    let Record::MemoryCondense { to_lsn, .. } = prepared.record() else {
        panic!("a condense record")
    };
    let to_lsn = *to_lsn;
    assert_eq!(
        fold(&mut memory, 30, prepared.record().clone()),
        MemoryApply::Applied
    );

    let summary = memory.summary(agent()).unwrap();
    assert_eq!(summary.lsn, Lsn(30));
    assert_eq!(summary.at, at(30));
    assert_eq!(summary.kind, MemorySummaryKind::Excerpts);
    assert_eq!(summary.sections, 16);
    assert_eq!(summary.bytes, 9 * 712 + 7 * 713);
    assert!(
        summary
            .text
            .starts_with("[Condensed memory: exact excerpts;")
    );
    assert_eq!(to_lsn, Lsn(16));

    let page = memory.page(agent(), None, 32, 2_048, &keep);
    assert_eq!(page.through, Some(Lsn(16)));
    assert_eq!(page.count, 20, "condense keeps raw entries");
    let condensed: Vec<_> = page
        .entries
        .iter()
        .map(|entry| (entry.lsn.0, entry.condensed))
        .collect();
    assert_eq!(
        &condensed[..5],
        [
            (20, false),
            (19, false),
            (18, false),
            (17, false),
            (16, true)
        ]
    );
    assert!(
        memory
            .entry(agent(), Lsn(3), 2_048, &keep)
            .unwrap()
            .condensed
    );
}
