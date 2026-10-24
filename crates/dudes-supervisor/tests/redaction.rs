//! A secret the daemon knows never reaches the journal in an agent's words.

use std::collections::BTreeMap;

use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Holder, Input, REDACTED};
use dudes_types::AgentId;

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
