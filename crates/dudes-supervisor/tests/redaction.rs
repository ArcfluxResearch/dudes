//! A secret the daemon knows never reaches the journal in an agent's words.

use std::collections::BTreeMap;

use dudes_proto::{
    Actor, Entry, Event, EventKind, ExitStatus, PeerProvenance, Record, TurnOutcome,
};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Holder, HookId, Input, REDACTED};
use dudes_types::{AgentId, Lsn, Provenance, SessionId, Timestamp, TurnId};

fn token_of(harness: &Harness) -> String {
    harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec),
            _ => None,
        })
        .map(|spec: &LaunchSpec| {
            spec.env
                .iter()
                .find(|var| var.name() == "DUDES_TOKEN")
                .and_then(|var| var.expose_value().to_str())
                .expect("the child was handed its token")
                .to_owned()
        })
        .expect("a child was launched")
}

fn finish(harness: &mut Harness, agent: AgentId, text: &str) {
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: Some(text.to_owned()),
    });
}

#[test]
fn the_last_message_is_scrubbed_before_the_turn_end_is_journaled() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let token = token_of(&harness);
    finish(&mut harness, agent, &format!("done, my token is {token}."));
    let journaled: Vec<_> = harness
        .journal
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::TurnEnded { last_message, .. } => last_message.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(journaled, [format!("done, my token is {REDACTED}.")]);
}

#[test]
fn a_question_is_scrubbed_before_the_human_question_is_journaled() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let token = token_of(&harness);
    harness.drive(Input::Command(Command::Ask {
        agent,
        text: format!("may I use {token}?"),
    }));
    let asked: Vec<_> = harness
        .journal
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HumanQuestion { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [format!("may I use {REDACTED}?")]);
}

#[test]
fn a_retired_agents_token_stops_being_a_secret() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let token = token_of(&harness);
    let redactor = harness.supervisor.redactor();
    assert_eq!(redactor.scrub(&token), REDACTED);
    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "done".into(),
        browser_retire: None,
    }));
    assert_eq!(redactor.scrub(&token), token);
}

#[test]
fn a_child_environment_is_held_until_its_turn_ends() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    let redactor = harness.supervisor.redactor();
    redactor.hold_env(
        Holder::Turn(turn),
        &[EnvVar::new("CLOUD_API_KEY", "key-live-8h2k9d0s7f6g")],
    );
    assert_eq!(
        redactor.scrub("use key-live-8h2k9d0s7f6g"),
        "use (redacted)"
    );
    finish(&mut harness, agent, "done");
    assert_eq!(
        redactor.scrub("use key-live-8h2k9d0s7f6g"),
        "use key-live-8h2k9d0s7f6g"
    );
}

fn journaled_turn_end(harness: &Harness) -> (Option<String>, Option<TurnOutcome>) {
    harness
        .journal
        .iter()
        .find_map(|entry| match &entry.record {
            Record::TurnEnded {
                last_message,
                outcome,
                ..
            } => Some((last_message.clone(), outcome.clone())),
            _ => None,
        })
        .expect("the turn ended")
}

fn end_with(harness: &mut Harness, agent: AgentId, outcome: TurnOutcome, last: Option<String>) {
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome,
        last_message: last,
    });
}

#[test]
fn a_secret_across_the_last_message_limit_is_replaced_before_the_cut() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let token = token_of(&harness);
    let text = format!("{}{token}{}", "a".repeat(4_096 - 10), "b".repeat(200));
    finish(&mut harness, agent, &text);
    let (last, _) = journaled_turn_end(&harness);
    let last = last.expect("a last message");
    assert!(last.len() <= 4_096, "{}", last.len());
    assert!(last.starts_with(&"a".repeat(4_000)));
    assert!(
        !last.contains(&token[..6]),
        "the journaled message kept the first part of the secret"
    );
    assert!(
        last.contains("(red"),
        "the replacement was cut, not the secret"
    );
}

#[test]
fn a_failed_reason_is_scrubbed_and_then_cut() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let token = token_of(&harness);
    let reason = format!(
        "boom {token} {}{token}{}",
        "a".repeat(4_096 - 60),
        "b".repeat(300)
    );
    end_with(&mut harness, agent, TurnOutcome::Failed { reason }, None);
    let (_, outcome) = journaled_turn_end(&harness);
    let Some(TurnOutcome::Failed { reason }) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.starts_with("boom (redacted) aaaa"), "{reason}");
    assert!(reason.len() <= 4_096);
    assert!(!reason.contains(&token[..6]), "{reason}");
}

#[test]
fn an_attached_hook_last_message_is_scrubbed_before_the_turn_end_is_journaled() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Replayed {
        lsn: Lsn(1),
        entry: Entry {
            at: Timestamp::from_unix_millis(1),
            record: Record::SessionLearned {
                agent,
                turn: TurnId::NIL,
                session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
            },
        },
    });
    harness.drive(Input::Command(Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
    let token = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::OpenPty { spec, .. } => spec
                .env
                .iter()
                .find(|var| var.name() == "DUDES_TOKEN")
                .and_then(|var| var.expose_value().to_str())
                .map(str::to_owned),
            _ => None,
        })
        .expect("the attached child was handed its token");
    let padding = "x".repeat(4_000);
    harness.drive(Input::Hook {
        hook: HookId(1),
        event: Event {
            agent,
            at: Timestamp::from_unix_millis(2),
            provenance: Provenance::Observed,
            kind: EventKind::TurnEnd {
                last_message: Some(format!("{padding} {token} tail")),
            },
            raw: None,
        },
    });
    let journaled: Vec<_> = harness
        .journal
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::TurnEnded { last_message, .. } => last_message.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(journaled.len(), 1, "the hook end was journaled");
    assert!(!journaled[0].contains(&token[..8]), "{:?}", journaled[0]);
    assert!(journaled[0].contains(REDACTED));
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A second private secret set would hide nothing from the readers that use the first, so only
/// the places that own a run may make one.
#[test]
fn only_the_run_owner_and_the_test_constructors_make_a_secret_set() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["crates", "bins"] {
        rust_files(&root.join(dir), &mut files);
    }
    let mut makers: Vec<String> = files
        .iter()
        .filter(|path| !path.components().any(|part| part.as_os_str() == "tests"))
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|text| text.contains("Redactor::for_run("))
        })
        .map(|path| {
            path.strip_prefix(&root)
                .expect("source below the repository root")
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    makers.sort();
    assert_eq!(
        makers,
        [
            "bins/duded/src/children.rs",
            "bins/duded/src/git_read/diff/reader.rs",
            "bins/duded/src/output.rs",
            "crates/dudes-supervisor/src/machine.rs",
        ],
    );
}
