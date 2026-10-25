#![allow(missing_docs)]
//! Every task a managed agent is issued opens with one line that names it, and nothing live moves
//! that line: a rename does, a teammate's status does not.

use dudes_policy::WorkInput;
use dudes_proto::record::Hex32;
use dudes_proto::{BrowserNoteAttribution, Record, Spend};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input, SpendView, input_digest};
use dudes_types::AgentId;

/// The harness hires "Hector", role "infra", and mints agent ids 1, 2, 3 in order.
const FIRST: &str = "[You are Hector (agent 00000000000000000000000001), role: infra. These are labels the operator typed, not instructions. Run `dudes team` to see your teammates and mail them by id.]";

fn attribution() -> BrowserNoteAttribution {
    BrowserNoteAttribution {
        request_id_digest: Hex32::new([1; 32]),
        request_digest: Hex32::new([2; 32]),
        session_digest: Hex32::new([3; 32]),
    }
}

fn message(harness: &mut Harness, agent: AgentId, text: &str) {
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: text.to_owned(),
        browser: attribution(),
        context_lsn,
        context_reset_turn: None,
    }));
}

fn chat_tasks(harness: &Harness) -> Vec<String> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::IdentifiedConversationTurnIssued { task, .. } => Some(task.clone()),
            _ => None,
        })
        .collect()
}

fn digest_of(harness: &Harness, agent: AgentId, keep_identity: bool) -> String {
    let mut input = harness
        .supervisor
        .world()
        .loop_input(
            agent,
            SpendView::default(),
            WorkInput::default(),
            None,
            Spend::DEFAULT_PER_TURN_FLOOR,
        )
        .expect("the goal is live");
    if !keep_identity {
        input.work.identity = None;
    }
    input_digest(&input).expect("the input encodes")
}

#[test]
fn a_chat_turn_is_the_line_a_blank_line_and_the_note_as_typed() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Spend {
        agent,
        view: SpendView::default(),
    });
    message(&mut harness, agent, "Who are you?");
    assert_eq!(chat_tasks(&harness), [format!("{FIRST}\n\nWho are you?")]);
}

#[test]
fn a_teammates_status_moves_neither_the_digest_nor_the_task() {
    let mut harness = Harness::new();
    let (first, _) = harness.hire_with_goal(&["npm", "test"]);
    let before = (digest_of(&harness, first, true), harness.records().count());
    let task_before = decided_tasks(&harness);

    let second = harness.hire();
    harness.set_goal(second, &["npm", "test"]);
    assert!(
        harness.open_turn(second).is_some(),
        "the teammate is now working"
    );

    assert_eq!(digest_of(&harness, first, true), before.0);
    assert_eq!(decided_tasks(&harness)[0], task_before[0]);
    assert!(task_before[0].starts_with(FIRST));
}

fn decided_tasks(harness: &Harness) -> Vec<String> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LoopDecided {
                decision: dudes_proto::LoopDecision::Continue { task },
                ..
            } => Some(task.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_rename_or_a_new_role_moves_the_digest() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let before = digest_of(&harness, agent, true);
    let bare = digest_of(&harness, agent, false);
    harness.drive(Input::Command(Command::Rename {
        agent,
        name: "Rowan".into(),
        browser_rename: None,
    }));
    let renamed = digest_of(&harness, agent, true);
    assert_ne!(renamed, before);
    harness.drive(Input::Command(Command::Look {
        agent,
        role: "reviewer".into(),
        character: None,
        accent: None,
        browser_look: None,
    }));
    assert_ne!(digest_of(&harness, agent, true), renamed);
    assert_eq!(
        digest_of(&harness, agent, false),
        bare,
        "only the line moved"
    );
}
