#![allow(missing_docs)]
//! `AgentMoved` in the saved agents projection: the places an agent keeps, the replay that
//! rebuilds them, and the older build that refuses the record.

use std::fs;
use std::path::Path;

use dudes_journal::frame::{Header, REQUIRED_VERSION, crc32c};
use dudes_journal::segment::segment_path;
use dudes_journal::{AgentRow, Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::placement::AgentMove;
use dudes_proto::record::{RecordedWorkspace, samples};
use dudes_proto::{AgentBudget, Entry, Record, RecordKind, WorktreeViewState};
use dudes_types::{AgentId, GitCommit, Lsn, SessionId, Timestamp, TurnId, WorktreeId};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn agent() -> AgentId {
    AgentId::from_parts(5, 1)
}

fn tree() -> WorktreeId {
    WorktreeId::from_bytes([0x11; 16])
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

fn workspace(id: &str) -> RecordedWorkspace {
    RecordedWorkspace {
        id: id.into(),
        label: id.into(),
    }
}

fn leased() -> Entry {
    entry(Record::WorktreeLeased {
        worktree: tree(),
        origin: "/repo/service".into(),
        path: "/data/copies/service".into(),
        branch: format!("dudes/{}", tree().short()),
        base: GitCommit::parse(BASE).unwrap(),
        from: None,
    })
}

fn hired(own_copy: bool) -> Entry {
    let mut hire = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id,
        cwd,
        workspace: project,
        worktree,
        trusted,
        ..
    } = &mut hire
    else {
        unreachable!()
    };
    *id = agent();
    *project = Some(workspace("service"));
    *trusted = true;
    if own_copy {
        *cwd = "/data/copies/service".into();
        *worktree = Some(tree());
    } else {
        *cwd = "/repo/service".into();
        *worktree = None;
    }
    entry(hire)
}

fn learned(session: &str) -> Entry {
    entry(Record::SessionLearned {
        agent: agent(),
        turn: TurnId::from_parts(5, 2),
        session: SessionId::new(session),
    })
}

fn moved(project: &str, cwd: &str, worktree: Option<WorktreeId>) -> Entry {
    entry(Record::AgentMoved {
        agent: agent(),
        to: AgentMove {
            cwd: cwd.into(),
            workspace: workspace(project),
            worktree,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
        },
    })
}

fn retired() -> Entry {
    entry(Record::AgentRetired {
        id: agent(),
        reason: "done".into(),
        browser_retire: None,
    })
}

/// Hired with its own copy in the first project, away and back.
fn story() -> Vec<Entry> {
    vec![
        leased(),
        hired(true),
        learned("service-session"),
        moved("billing", "/repo/billing", None),
        learned("billing-session"),
        moved("service", "/data/copies/service", Some(tree())),
    ]
}

fn options(checkpoint_every: u64) -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        checkpoint_every,
        ..JournalOptions::default()
    }
}

fn write_journal(dir: &Path, entries: &[Entry], checkpoint_every: u64) {
    let (mut writer, _) = open(dir, options(checkpoint_every)).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
}

fn replayed(dir: &Path) -> Projections {
    let (reader, _) = Reader::open(dir).unwrap();
    let mut store = Projections::in_memory().unwrap();
    replay(&reader, &mut store).unwrap();
    store
}

fn applied_one_at_a_time(dir: &Path) -> Projections {
    let (reader, _) = Reader::open(dir).unwrap();
    let mut store = Projections::in_memory().unwrap();
    for item in reader.iter_from(Lsn::ZERO).unwrap() {
        let (lsn, entry) = item.unwrap();
        store.apply(lsn, 0, &entry).unwrap();
    }
    store
}

fn row(store: &Projections) -> AgentRow {
    store.agent(agent()).unwrap().unwrap()
}

fn cwd(row: &AgentRow) -> &Path {
    row.repositories.singleton().unwrap().cwd()
}

#[test]
fn a_move_to_a_new_place_is_fresh_and_a_return_restores_the_stored_one() {
    let mut store = Projections::in_memory().unwrap();
    let steps = story();
    for (index, entry) in steps.iter().take(4).enumerate() {
        store.apply(Lsn(index as u64 + 1), 0, entry).unwrap();
    }
    let away = row(&store);
    assert_eq!(cwd(&away), Path::new("/repo/billing"));
    assert!(!away.trusted);
    assert_eq!((away.session.as_ref(), away.session_lsn), (None, None));
    let stored = &away.other_projects["service"];
    assert!(stored.trusted);
    assert_eq!(stored.session, Some(SessionId::new("service-session")));
    assert_eq!(stored.session_lsn, Some(Lsn(3)));
    assert_eq!(stored.repository.worktree(), Some(tree()));

    for (index, entry) in steps.iter().enumerate().skip(4) {
        store.apply(Lsn(index as u64 + 1), 0, entry).unwrap();
    }
    let back = row(&store);
    assert_eq!(cwd(&back), Path::new("/data/copies/service"));
    assert_eq!(back.repositories.primary_worktree(), Some(tree()));
    assert!(back.trusted);
    assert_eq!(back.session, Some(SessionId::new("service-session")));
    assert_eq!(back.session_lsn, Some(Lsn(3)));
    assert_eq!(
        back.other_projects["billing"].session,
        Some(SessionId::new("billing-session"))
    );
    assert!(!back.other_projects.contains_key("service"));
}

#[test]
fn each_move_leaves_one_line_in_the_agents_chat_and_a_rebuild_keeps_them() {
    for checkpoint_every in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), &story(), checkpoint_every);
        let live = applied_one_at_a_time(dir.path());
        let rebuilt = replayed(dir.path());
        let turns = rebuilt.chat().unwrap().turns;
        let lines: Vec<_> = turns
            .iter()
            .map(|turn| {
                assert_eq!(turn.agent_id, agent());
                assert_eq!(
                    (&turn.turn_id, &turn.text, &turn.cause, &turn.notice),
                    (&None, &None, &None, &None)
                );
                turn.moved.as_ref().unwrap().project.as_str()
            })
            .collect();
        assert_eq!(lines, ["billing", "service"]);
        assert_ne!(turns[0].note_lsn, turns[1].note_lsn);
        assert_eq!(live.chat().unwrap(), rebuilt.chat().unwrap());
    }
}

#[test]
fn replay_from_the_journal_reaches_what_applying_each_record_reached() {
    for checkpoint_every in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), &story(), checkpoint_every);
        let live = applied_one_at_a_time(dir.path());
        let rebuilt = replayed(dir.path());
        let moved_back = row(&rebuilt);
        assert_eq!(
            moved_back.other_projects.keys().collect::<Vec<_>>(),
            ["billing"],
            "a control: the story left a place behind"
        );
        assert_eq!(
            row(&live),
            moved_back,
            "checkpoint_every {checkpoint_every}"
        );
        assert_eq!(live.agents().unwrap(), rebuilt.agents().unwrap());
    }
}

#[test]
fn a_cache_file_rebuilt_from_nothing_matches_the_one_that_followed_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &story(), 0);
    let cache = dir.path().join("state.redb");
    let first = {
        let (reader, _) = Reader::open(dir.path()).unwrap();
        let mut store = Projections::open(&cache).unwrap();
        replay(&reader, &mut store).unwrap();
        store.agents().unwrap()
    };
    fs::remove_file(&cache).unwrap();
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let mut store = Projections::open(&cache).unwrap();
    replay(&reader, &mut store).unwrap();
    assert_eq!(store.agents().unwrap(), first);
    assert!(!first[0].other_projects.is_empty());
}

#[test]
fn a_row_written_before_moves_existed_reads_with_no_other_places() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &story()[..2], 0);
    let rebuilt = replayed(dir.path());
    let mut value = serde_json::to_value(row(&rebuilt)).unwrap();
    value.as_object_mut().unwrap().remove("trusted");
    value.as_object_mut().unwrap().remove("other_projects");
    let older: AgentRow = serde_json::from_value(value).unwrap();
    assert!(older.other_projects.is_empty());
    assert!(!older.trusted);
}

#[test]
fn a_retired_agents_copy_is_released_and_listed_while_it_sits_in_a_stored_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = story();
    steps.truncate(4);
    steps.push(retired());
    write_journal(dir.path(), &steps, 0);
    let store = replayed(dir.path());
    assert!(store.agent(agent()).unwrap().is_none());
    let listed = store.retired_agents().unwrap();
    let copy = listed[0].worktree.as_ref().expect("the copy is listed");
    assert_eq!(copy.state, WorktreeViewState::Released);
    let lease = &store.worktrees().unwrap()[0];
    assert!(lease.agent_retired);
    assert_eq!(lease.agent, Some(agent()));
}

#[test]
fn an_older_build_refuses_the_record_by_its_kind_and_this_build_reads_it() {
    let record = moved("billing", "/repo/billing", None).record;
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &[entry(record.clone())], 0);
    let path = segment_path(dir.path(), 1);
    let bytes = fs::read(&path).unwrap();
    let header = Header::parse(bytes[..12].try_into().unwrap());
    assert_eq!(header.kind, RecordKind::AgentMoved.tag());
    assert_eq!(header.kind, 116);
    assert_eq!(header.ver, REQUIRED_VERSION);
    let (reader, _) = Reader::open(dir.path()).expect("this build understands it");
    let read: Vec<_> = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read[0].1.record, record);

    let older = dir.path().join("older");
    fs::create_dir(&older).unwrap();
    let payload = &bytes[12..];
    let kind = 200 + header.kind;
    let mut frame = Header {
        len: u32::try_from(payload.len()).unwrap(),
        ver: header.ver,
        kind,
        crc: crc32c(header.ver, kind, payload),
    }
    .to_bytes()
    .to_vec();
    frame.extend_from_slice(payload);
    fs::write(segment_path(&older, 1), frame).unwrap();
    let error = Reader::open(&older).unwrap_err().to_string();
    assert!(
        error.starts_with("journal required record kind 316 at 1:"),
        "{error}"
    );
}

#[test]
fn a_journal_without_the_record_opens_and_replays_as_before() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &story()[..3], 0);
    let rebuilt = replayed(dir.path());
    let unmoved = row(&rebuilt);
    assert!(unmoved.other_projects.is_empty());
    assert_eq!(unmoved.session, Some(SessionId::new("service-session")));
    assert_eq!(unmoved.repositories.primary_worktree(), Some(tree()));
}
