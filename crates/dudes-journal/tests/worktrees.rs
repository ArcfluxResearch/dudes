#![allow(missing_docs)]
//! the lease records, their projection and their compatibility.
//!
//! A lease is a record, so replay rebuilds the table, a checkpoint the writer adds changes
//! nothing about it, and a reader that does not know the two kinds skips their frames instead of
//! refusing the journal.

use std::fs;
use std::path::Path;

use dudes_journal::frame::{Header, crc32c};
use dudes_journal::segment::segment_path;
use dudes_journal::{Fsync, JournalOptions, Projections, Reader, WorktreeRow, open, replay};
use dudes_proto::record::samples;
use dudes_proto::{AutomaticHold, Entry, Record, RecordKind, WorktreeRemoval};
use dudes_types::{AgentId, GitCommit, Lsn, Timestamp, WorktreeId};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

fn tree() -> WorktreeId {
    WorktreeId::from_bytes([0x11; 16])
}

fn based_on_tree() -> WorktreeId {
    WorktreeId::from_bytes([0x22; 16])
}

fn leased(worktree: WorktreeId, from: Option<WorktreeId>) -> Entry {
    entry(Record::WorktreeLeased {
        worktree,
        origin: "/repo".into(),
        path: format!("/data/worktrees/{worktree}").into(),
        branch: format!("dudes/{}", worktree.short()),
        base: GitCommit::parse(BASE).unwrap(),
        from,
    })
}

fn hired(agent: AgentId, worktree: Option<WorktreeId>) -> Entry {
    let mut hire = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id, worktree: slot, ..
    } = &mut hire
    else {
        unreachable!()
    };
    *id = agent;
    *slot = worktree;
    entry(hire)
}

fn retired(agent: AgentId) -> Entry {
    entry(Record::AgentRetired {
        id: agent,
        reason: "done".into(),
        browser_retire: None,
    })
}

fn removed(worktree: WorktreeId, how: WorktreeRemoval) -> Entry {
    entry(Record::WorktreeRemoved { worktree, how })
}

fn lifecycle(agent: AgentId) -> Vec<Entry> {
    vec![
        leased(tree(), None),
        leased(based_on_tree(), Some(tree())),
        hired(agent, Some(tree())),
        retired(agent),
        removed(tree(), WorktreeRemoval::Operator),
        removed(based_on_tree(), WorktreeRemoval::NeverAssigned),
    ]
}

fn apply_all(store: &mut Projections, entries: &[Entry]) {
    for (index, entry) in entries.iter().enumerate() {
        let lsn = Lsn(u64::try_from(index).unwrap() + 1);
        assert!(store.apply(lsn, 7, entry).unwrap());
    }
}

#[test]
fn the_projection_follows_a_lease_from_pending_to_removed() {
    let agent = AgentId::from_parts(5, 1);
    let mut store = Projections::in_memory().unwrap();
    let steps = lifecycle(agent);

    apply_all(&mut store, &steps[..2]);
    let rows = store.worktrees().unwrap();
    assert_eq!(rows.len(), 2);
    let first: &WorktreeRow = rows.iter().find(|row| row.worktree == tree()).unwrap();
    assert_eq!(first.agent, None, "no hire names it yet");
    assert_eq!(first.removed, None);
    assert_eq!(first.branch, format!("dudes/{}", tree().short()));
    let second = rows
        .iter()
        .find(|row| row.worktree == based_on_tree())
        .unwrap();
    assert_eq!(second.from, Some(tree()));
    assert_eq!(second.base.to_string(), BASE);

    store.apply(Lsn(3), 7, &steps[2]).unwrap();
    let hired = store.agent(agent).unwrap().expect("the agent is projected");
    assert_eq!(hired.worktree, Some(tree()));
    let owner = |store: &Projections| {
        store
            .worktrees()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == tree())
            .unwrap()
    };
    assert_eq!(owner(&store).agent, Some(agent));
    assert!(!owner(&store).agent_retired);

    store.apply(Lsn(4), 7, &steps[3]).unwrap();
    assert!(owner(&store).agent_retired);
    assert_eq!(owner(&store).removed, None, "retirement removes nothing");

    store.apply(Lsn(5), 7, &steps[4]).unwrap();
    store.apply(Lsn(6), 7, &steps[5]).unwrap();
    let ended: Vec<_> = store
        .worktrees()
        .unwrap()
        .into_iter()
        .map(|row| (row.worktree, row.removed))
        .collect();
    assert_eq!(
        ended,
        [
            (tree(), Some(WorktreeRemoval::Operator)),
            (based_on_tree(), Some(WorktreeRemoval::NeverAssigned)),
        ]
    );
}

#[test]
fn a_hire_into_no_worktree_and_a_second_hire_into_a_taken_one_leave_the_owner_alone() {
    let (first, second, plain) = (
        AgentId::from_parts(5, 1),
        AgentId::from_parts(5, 2),
        AgentId::from_parts(5, 3),
    );
    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[
            leased(tree(), None),
            hired(first, Some(tree())),
            hired(second, Some(tree())),
            hired(plain, None),
            retired(second),
        ],
    );
    let row = &store.worktrees().unwrap()[0];
    assert_eq!(row.agent, Some(first));
    assert!(
        !row.agent_retired,
        "the retirement of an agent that does not own the lease says nothing about it"
    );
    assert_eq!(store.agent(plain).unwrap().unwrap().worktree, None);
}

fn journal_options(checkpoint_every: u64) -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        checkpoint_every,
        ..JournalOptions::default()
    }
}

fn write_journal(dir: &Path, entries: &[Entry], checkpoint_every: u64) {
    let (mut writer, _) = open(dir, journal_options(checkpoint_every)).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
}

fn replayed(dir: &Path) -> Vec<WorktreeRow> {
    let (reader, _) = Reader::open(dir).unwrap();
    let mut store = Projections::in_memory().unwrap();
    replay(&reader, &mut store).unwrap();
    store.worktrees().unwrap()
}

#[test]
fn replay_rebuilds_the_table_and_the_writers_checkpoints_change_nothing() {
    let steps = lifecycle(AgentId::from_parts(5, 1));
    let plain = tempfile::tempdir().unwrap();
    write_journal(plain.path(), &steps, 0);
    let checkpointed = tempfile::tempdir().unwrap();
    write_journal(checkpointed.path(), &steps, 1);

    let rows = replayed(plain.path());
    assert_eq!(rows.len(), 2, "a control: the table is not empty");
    assert_eq!(
        rows.iter().filter(|row| row.removed.is_some()).count(),
        2,
        "a control: the removals reached the table"
    );
    let with_checkpoints = replayed(checkpointed.path());
    assert_eq!(with_checkpoints, rows);
}

fn reframed(bytes: &[u8], from: u16, to: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let header = Header::parse(bytes[at..at + 12].try_into().unwrap());
        let payload = &bytes[at + 12..at + 12 + header.len as usize];
        let kind = if header.kind == from { to } else { header.kind };
        let rebuilt = Header {
            len: header.len,
            ver: header.ver,
            kind,
            crc: crc32c(header.ver, kind, payload),
        };
        out.extend_from_slice(&rebuilt.to_bytes());
        out.extend_from_slice(payload);
        at += 12 + header.len as usize;
    }
    out
}

/// the two kinds are compatible, frame `ver = 1`. A daemon from before them meets
/// the lease frames as kinds it does not know, skips them by length and goes on, which this stands
/// in for by renumbering the frames to a kind nothing knows. The hire that names the lease is a
/// kind it does know and still replays, and none of the file is rewritten.
#[test]
fn a_reader_that_does_not_know_the_lease_kinds_skips_their_frames_and_changes_nothing() {
    let agent = AgentId::from_parts(5, 1);
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &lifecycle(agent), 0);
    let path = segment_path(dir.path(), 1);
    let written = fs::read(&path).unwrap();
    let leased_tag = RecordKind::WorktreeLeased.tag();
    let removed_tag = RecordKind::WorktreeRemoved.tag();
    assert_eq!((leased_tag, removed_tag), (67, 68));
    let older = reframed(&reframed(&written, leased_tag, 1_000), removed_tag, 1_001);
    assert_ne!(older, written, "a control: the frames were renumbered");
    fs::write(&path, &older).unwrap();

    let (reader, report) = Reader::open(dir.path()).expect("unknown compatible kinds are skipped");
    assert_eq!(report.frames, 6);
    let mut store = Projections::in_memory().unwrap();
    let applied = replay(&reader, &mut store).unwrap();
    assert_eq!(
        applied.applied, 2,
        "the hire and the retirement still replay"
    );
    assert!(store.worktrees().unwrap().is_empty());
    assert_eq!(
        store.agent(agent).unwrap(),
        None,
        "the agent retired after its hire"
    );
    drop(reader);
    let (_, reopened) = open(dir.path(), JournalOptions::default()).unwrap();
    assert_eq!(reopened.frames, 6);
    assert_eq!(
        fs::read(&path).unwrap(),
        older,
        "nothing was repaired or rewritten"
    );
}

#[test]
fn the_removal_cause_and_the_automatic_hold_keep_a_value_a_later_version_adds() {
    let future: WorktreeRemoval = serde_json::from_str("\"reclaimed_by_a_later_version\"").unwrap();
    assert_eq!(future, WorktreeRemoval::Unknown);
    let hold: AutomaticHold =
        serde_json::from_value(serde_json::json!({"kind": "a_later_cause", "detail": 1})).unwrap();
    assert_eq!(hold, AutomaticHold::Unknown);
    let missing = AutomaticHold::WorktreeMissing { worktree: tree() };
    let encoded = serde_json::to_value(&missing).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({"kind": "worktree_missing", "worktree": tree().to_string()})
    );
    assert_eq!(
        serde_json::from_value::<AutomaticHold>(encoded).unwrap(),
        missing
    );
}

/// A cache written before the lease table existed has a cursor past every lease record, so
/// ordinary replay would skip them for ever. The version bump is what rebuilds it.
#[test]
fn a_version_37_cache_is_rebuilt_so_it_holds_the_leases_the_journal_records() {
    assert_eq!(dudes_journal::PROJECTION_VERSION, 44);
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    write_journal(&journal, &lifecycle(AgentId::from_parts(5, 1)), 0);
    let (reader, _) = Reader::open(&journal).unwrap();
    let mut fresh = Projections::in_memory().unwrap();
    replay(&reader, &mut fresh).unwrap();
    assert_eq!(
        fresh.worktrees().unwrap().len(),
        2,
        "a control: the journal holds leases"
    );
    let expected = fresh.export().unwrap();

    let cache = dir.path().join("state.redb");
    let mut cached = Projections::open(&cache).unwrap();
    cached
        .apply(
            Lsn(u64::MAX - 1),
            17,
            &entry(samples::record(RecordKind::Checkpoint)),
        )
        .unwrap();
    drop(cached);
    let database = redb::Database::create(&cache).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("meta"))
        .unwrap()
        .insert("schema_version", &37)
        .unwrap();
    transaction.commit().unwrap();
    drop(database);

    let mut rebuilt = Projections::open(&cache).unwrap();
    assert_eq!(
        rebuilt.last_applied_lsn(),
        Lsn::ZERO,
        "the old cache was dropped"
    );
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), expected);
    assert_eq!(rebuilt.worktrees().unwrap().len(), 2);
}

#[test]
fn a_hire_writes_its_lease_only_when_it_has_one_and_an_older_hire_reads_as_none() {
    let agent = AgentId::from_parts(5, 1);
    let plain = serde_json::to_value(hired(agent, None)).unwrap();
    assert!(plain.get("worktree").is_none());
    let into_tree = serde_json::to_value(hired(agent, Some(tree()))).unwrap();
    assert_eq!(into_tree["worktree"], tree().to_string());
    let decoded: Entry = serde_json::from_value(plain).unwrap();
    assert!(matches!(
        decoded.record,
        Record::AgentHired { worktree: None, .. }
    ));
    let decoded: Entry = serde_json::from_value(into_tree).unwrap();
    assert!(matches!(
        decoded.record,
        Record::AgentHired { worktree: Some(found), .. } if found == tree()
    ));
}

/// the archive shows a retired agent's tree until the operator removes it, and the
/// Floor's view never carries a path.
#[test]
fn the_archive_keeps_a_released_tree_visible_until_it_is_removed() {
    let agent = AgentId::from_parts(5, 1);
    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[
            leased(tree(), None),
            hired(agent, Some(tree())),
            retired(agent),
        ],
    );
    let archived = store.retired_agents().unwrap();
    let view = archived[0]
        .worktree
        .clone()
        .expect("the retired agent's worktree");
    assert_eq!(view.branch, format!("dudes/{}", tree().short()));
    assert_eq!(view.state, dudes_proto::WorktreeViewState::Released);
    assert!(
        !serde_json::to_string(&archived)
            .unwrap()
            .contains("/data/worktrees")
    );

    store
        .apply(Lsn(4), 7, &removed(tree(), WorktreeRemoval::Operator))
        .unwrap();
    assert_eq!(
        store.retired_agents().unwrap()[0]
            .worktree
            .as_ref()
            .unwrap()
            .state,
        dudes_proto::WorktreeViewState::Removed
    );
}
