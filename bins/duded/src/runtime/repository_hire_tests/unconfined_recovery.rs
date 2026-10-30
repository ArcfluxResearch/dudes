//! A daemon restart settles an unconfined multi-repository hire's interrupted turn the way it
//! settles a single-repository one.

use super::*;
use crate::floor_chat::{ChatOutcome, ChatRequest, ChatVerb};
use dudes_proto::AutomaticHold;
use dudes_types::{AgentId, TurnId};

fn send(running: &common::Running, agent: AgentId) {
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::new(common::TestClock::new()),
    );
    let session = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    let body = serde_json::json!({
        "request_id": "A".repeat(43),
        "agent": agent.to_string(),
        "text": "hello",
    });
    let request = ChatRequest::parse(ChatVerb::Send, &serde_json::to_vec(&body).unwrap()).unwrap();
    assert!(matches!(
        running.handle.chat(session, request).unwrap(),
        ChatOutcome::Queued { .. }
    ));
}

fn interrupted(setup: &Setup) -> (AgentId, TurnId) {
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    send(&running, agent);
    let turn = setup
        .fixture
        .await_record("the spawn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    running.stop(ExitCause::Ended);
    (agent, turn)
}

fn cut_before_spawn(setup: &Setup) {
    let kept: Vec<_> = dudes_journal::Reader::new(&setup.fixture.journal_dir())
        .iter_from(dudes_types::Lsn::ZERO)
        .unwrap()
        .map(Result::unwrap)
        .take_while(|(_, entry)| !matches!(entry.record, Record::HookTokenMinted { .. }))
        .collect();
    std::fs::remove_dir_all(setup.fixture.journal_dir()).unwrap();
    std::fs::remove_file(
        setup
            .fixture
            .state
            .path()
            .join(crate::boot::PROJECTION_FILE),
    )
    .ok();
    for (index, chunk) in [&kept[..1], &kept[1..]].into_iter().enumerate() {
        let (mut writer, _) =
            dudes_journal::open(&setup.fixture.journal_dir(), setup.settings.journal).unwrap();
        for (old, entry) in chunk {
            assert_eq!(writer.append(entry).unwrap(), *old, "chunk {index}");
        }
    }
}

fn count(setup: &Setup, f: impl Fn(&Record) -> bool) -> usize {
    setup.fixture.records().iter().filter(|r| f(r)).count()
}

#[test]
fn a_child_gone_with_the_daemon_is_settled_once_and_nothing_launches_again() {
    let setup = Setup::with_repositories(&["web", "api"]);
    let (_, turn) = interrupted(&setup);
    let mut again = common::start(setup.boot_unconfined());
    setup
        .fixture
        .await_record("the vanished end", |record| match record {
            Record::TurnEnded { turn: ended, .. } if *ended == turn => Some(()),
            _ => None,
        });
    setup
        .fixture
        .await_record("the settled launch boundary", |record| {
            matches!(record, Record::InstructionObservationBatchFinished(_)).then_some(())
        });
    again.stop(ExitCause::Ended);
    assert_eq!(setup.host.spawned().len(), 1, "no second child");
    assert_eq!(count(&setup, |r| matches!(r, Record::TurnEnded { .. })), 1);
    assert_eq!(
        count(&setup, |r| matches!(
            r,
            Record::TurnIssuedWithRepositories(_)
        )),
        1
    );
}

#[test]
fn a_copy_missing_at_restart_holds_the_agent_instead_of_recovering() {
    let setup = Setup::with_repositories(&["web", "api"]);
    let (agent, _) = interrupted(&setup);
    cut_before_spawn(&setup);
    let copy = setup
        .fixture
        .records()
        .iter()
        .filter_map(Record::worktree_lease)
        .map(|lease| (lease.worktree, lease.path.to_path_buf()))
        .nth(1)
        .unwrap();
    std::fs::rename(&copy.1, copy.1.with_extension("moved")).unwrap();
    let mut again = common::start(setup.boot_unconfined());
    let held = setup
        .fixture
        .await_record("the hold", |record| match record {
            Record::AgentHeld {
                agent: owner,
                cause: AutomaticHold::WorktreeMissing { worktree },
                ..
            } if *owner == agent => Some(*worktree),
            _ => None,
        });
    again.stop(ExitCause::Ended);
    assert_eq!(held, copy.0);
    assert_eq!(setup.host.spawned().len(), 1, "no second child");
}
