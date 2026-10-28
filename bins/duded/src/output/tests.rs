use dudes_providers::steps::StepEvent;
use dudes_supervisor::Redactor;
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{AgentId, SessionId, TurnId};

use super::ring::*;
use super::state::*;
use crate::step_results::Fetch;
use crate::steps;

use super::*;

#[test]
fn provider_omissions_follow_pending_confirmation_and_session_gates() {
    for before_registration in [false, true] {
        for session in ["wrong", "session"] {
            let output = OutputRegistry::new();
            let agent = AgentId::from_parts(1, 1);
            let turn = TurnId::from_parts(2, 1);
            let key = AdoptionKey {
                pid: 10,
                start_time: 20,
                boot: None,
            };
            if before_registration {
                for _ in 0..1000 {
                    output.omit(&key, session);
                }
            }
            output.replace(agent, turn);
            assert!(output.register(agent, turn, &key));
            if !before_registration {
                for _ in 0..1000 {
                    output.omit(&key, session);
                }
            }
            output.capture(&key, "session", "prefix");
            assert!(output.snapshot(agent).reply.is_none());
            output.confirm(agent, turn, &SessionId::new("session"));
            let reply = output.snapshot(agent).reply.unwrap();
            assert_eq!(reply.text, "prefix");
            assert_eq!(
                reply.loss,
                if session == "session" {
                    Some(ReplyLoss::ProviderLimit)
                } else {
                    None
                }
            );
            assert_eq!(output.usage().chunks, 1);
            assert_eq!(output.usage().text_bytes, 12);
        }
    }
}

#[test]
fn provider_omissions_preserve_text_and_refuse_stale_children_and_ended_turns() {
    let output = OutputRegistry::new();
    let agent = AgentId::from_parts(3, 1);
    let turn = TurnId::from_parts(4, 1);
    let key = AdoptionKey {
        pid: 30,
        start_time: 40,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new("session"));
    output.capture(&key, "session", "first");
    let before = output.snapshot(agent).reply.unwrap();
    let recycled = AdoptionKey {
        start_time: 41,
        ..key.clone()
    };
    output.omit(&recycled, "session");
    output.omit(&key, "wrong");
    assert_eq!(output.snapshot(agent).reply.unwrap(), before);
    output.omit(&key, "session");
    let lost = output.snapshot(agent).reply.unwrap();
    assert_eq!(lost.text, "first");
    assert_eq!(lost.loss, Some(ReplyLoss::ProviderLimit));
    assert!(lost.revision > before.revision);
    output.capture(&key, "session", " reply");
    assert_eq!(output.snapshot(agent).reply.unwrap().text, "first reply");
    output.end(agent, turn);
    let ended = output.snapshot(agent).reply.unwrap();
    let refused = output.usage().refused;
    output.omit(&key, "session");
    assert_eq!(output.snapshot(agent).reply.unwrap(), ended);
    assert_eq!(output.usage().refused, refused + 1);
    output.retire(agent);
    output.omit(&key, "session");
    assert!(output.snapshot(agent).reply.is_none());
    assert_eq!(output.usage().refused, refused + 2);
}

#[test]
fn repeated_pending_omissions_preserve_text_with_bounded_identity_storage() {
    let output = OutputRegistry::new();
    let key = AdoptionKey {
        pid: 50,
        start_time: 60,
        boot: None,
    };
    output.capture(&key, "session", "prefix");
    for _ in 0..1000 {
        output.omit(&key, "session");
    }
    assert_eq!(output.usage().pending_keys, 1);
    assert_eq!(output.usage().chunks, 2);
    assert_eq!(output.usage().text_bytes, 6);
    assert_eq!(output.usage().identity_bytes, 2 * 7);
    let agent = AgentId::from_parts(5, 1);
    let turn = TurnId::from_parts(6, 1);
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new("session"));
    let reply = output.snapshot(agent).reply.unwrap();
    assert_eq!(reply.text, "prefix");
    assert_eq!(reply.loss, Some(ReplyLoss::ProviderLimit));
    assert_eq!(output.usage().chunks, 1);
}

#[test]
fn utf8_split_and_checked_sequence_overflow_are_explicit() {
    let text = "🙂".repeat(1_025);
    let pieces: Vec<_> = split_utf8(&text).collect();
    assert_eq!(
        pieces.iter().map(|s| s.len()).collect::<Vec<_>>(),
        [4_096, 4]
    );
    let mut ring = Ring::new(TurnId::NIL, 0);
    ring.next_seq = u64::MAX;
    ring.add_visible("x".into(), 1);
    assert!(ring.overflow);
    assert_eq!(ring.loss, Some(Loss::SequenceOverflow));
    assert!(ring.chunks.is_empty());
}

use dudes_providers::steps::{Step, StepKind};

const SESSION: &str = "session";

fn open_event(id: &str, session: &str, command: &str) -> StepEvent {
    StepEvent::Open {
        session: SessionId::new(session),
        step: Step {
            id: id.to_owned(),
            parent: None,
            kind: StepKind::Command,
            tool: "Bash".to_owned(),
            args: vec![("command".to_owned(), command.to_owned())],
            edit: None,
            plan: Vec::new(),
        },
    }
}

fn close_event(id: &str) -> StepEvent {
    StepEvent::Close {
        session: SessionId::new(SESSION),
        id: id.to_owned(),
        failed: false,
        exit: None,
        result_bytes: 7,
        result: None,
    }
}

fn close_with(id: &str, text: &str) -> StepEvent {
    StepEvent::Close {
        session: SessionId::new(SESSION),
        id: id.to_owned(),
        failed: false,
        exit: None,
        result_bytes: text.len() as u64,
        result: Some(text.to_owned()),
    }
}

fn bound() -> (OutputRegistry, AgentId, TurnId, AdoptionKey) {
    let output = OutputRegistry::new();
    let agent = AgentId::from_parts(1, 1);
    let turn = TurnId::from_parts(2, 1);
    let key = AdoptionKey {
        pid: 10,
        start_time: 20,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    (output, agent, turn, key)
}

#[test]
fn steps_wait_for_the_confirmed_session_and_then_appear_once() {
    let (output, agent, turn, key) = bound();
    output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
    output.capture_step(&key, close_event("toolu_1"));
    output.capture_step(&key, open_event("toolu_2", "another", "pwd"));
    assert!(output.snapshot(agent).steps.is_none());
    assert!(output.steps_since(agent, 0).is_none());
    output.confirm(agent, turn, &SessionId::new(SESSION));
    let delta = output.steps_since(agent, 0).expect("steps once confirmed");
    let ids: Vec<_> = delta.items.iter().map(|step| step.id.as_str()).collect();
    assert_eq!(ids, ["toolu_1"], "a line of another session is refused");
    assert_eq!(delta.items[0].result_bytes, Some(7));
    assert_eq!(output.snapshot(agent).steps.unwrap().seq, delta.seq);
    assert!(
        output
            .steps_since(agent, delta.seq)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn steps_that_arrive_before_the_turn_is_registered_join_it_later() {
    let output = OutputRegistry::new();
    let agent = AgentId::from_parts(1, 1);
    let turn = TurnId::from_parts(2, 1);
    let key = AdoptionKey {
        pid: 10,
        start_time: 20,
        boot: None,
    };
    output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new(SESSION));
    assert_eq!(output.steps_since(agent, 0).unwrap().items.len(), 1);
}

#[test]
fn a_flood_of_steps_never_evicts_the_reply_text() {
    let (output, agent, turn, key) = bound();
    output.capture(&key, SESSION, "the answer");
    output.confirm(agent, turn, &SessionId::new(SESSION));
    for index in 0..600 {
        output.capture_step(&key, open_event(&format!("toolu_{index}"), SESSION, "ls"));
    }
    let delta = output.steps_since(agent, 0).unwrap();
    assert_eq!(delta.items.len(), STEP_RING_ENTRIES);
    assert!(delta.truncated);
    assert_eq!(delta.items[0].id, "toolu_344", "the oldest steps go first");
    assert_eq!(output.snapshot(agent).reply.unwrap().text, "the answer");
}

#[test]
fn the_end_of_a_turn_settles_the_steps_still_running() {
    let (output, agent, turn, key) = bound();
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(&key, open_event("toolu_1", SESSION, "sleep 60"));
    output.end(agent, turn);
    let step = &output.steps_since(agent, 0).unwrap().items[0];
    assert_eq!(step.status, steps::Status::Failed);
    assert_eq!(step.note, Some("the turn ended first"));
}

fn heavy_event(id: &str) -> StepEvent {
    StepEvent::Open {
        session: SessionId::new(SESSION),
        step: Step {
            id: id.to_owned(),
            parent: None,
            kind: StepKind::Other,
            tool: "Custom".to_owned(),
            args: (0..8)
                .map(|n| (format!("arg{n}"), "y".repeat(steps::INPUT_BYTES)))
                .collect(),
            edit: None,
            plan: Vec::new(),
        },
    }
}

#[test]
fn steps_of_every_kind_share_the_run_cap_and_the_oldest_orphan_goes_first() {
    const PER_ORPHAN: usize = 10;
    const WAITING: usize = 5;
    let output = OutputRegistry::new();
    let key_at = |pid: u32| AdoptionKey {
        pid,
        start_time: 20,
        boot: None,
    };
    // Capturing is linear in bytes, so capture only enough to pass the cap by two orphans' worth.
    let probe = OutputRegistry::new();
    probe.capture_step(&key_at(1), heavy_event("toolu_probe"));
    let one_step = probe.state().step_bytes();
    let orphans = (MAX_BYTES / one_step + 2 * PER_ORPHAN).div_ceil(PER_ORPHAN);
    let orphans = u32::try_from(orphans).unwrap();
    for pid in 1..=orphans {
        for step in 0..PER_ORPHAN {
            output.capture_step(&key_at(pid), heavy_event(&format!("toolu_{pid}_{step}")));
        }
    }
    let (agent, turn) = (AgentId::from_parts(1, 1), TurnId::from_parts(2, 1));
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key_at(1_000)));
    for step in 0..WAITING {
        output.capture_step(&key_at(1_000), heavy_event(&format!("toolu_w_{step}")));
    }
    let state = output.state();
    assert!(
        state.step_bytes() + state.text_bytes() <= MAX_BYTES,
        "{} bytes held",
        state.step_bytes()
    );
    let held = |pid: u32| {
        state
            .orphan
            .get(&ChildKey::from(&key_at(pid)))
            .map_or(0, |orphan| orphan.steps.items.len())
    };
    assert_eq!(held(1), 0, "the oldest steps-only orphan is evicted first");
    assert!(held(orphans) > 0, "the newest orphan keeps its steps");
    assert!(
        !state.rings[&agent].pending_steps.items.is_empty(),
        "the waiting steps of a bound turn are counted and kept while newest"
    );
}

#[test]
fn a_secret_learned_after_capture_is_taken_out_when_steps_are_read() {
    let redactor = Redactor::for_run();
    let output = OutputRegistry::with_redactor(redactor.clone());
    let (agent, turn) = (AgentId::from_parts(1, 1), TurnId::from_parts(2, 1));
    let key = AdoptionKey {
        pid: 10,
        start_time: 20,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(
        &key,
        open_event("toolu_1", SESSION, "echo late-secret-0123456789"),
    );
    redactor.set_source("test", ["late-secret-0123456789".to_owned()]);
    let step = &output.steps_since(agent, 0).unwrap().items[0];
    assert_eq!(step.args[0].value, "echo (redacted)");
}

#[test]
fn a_secret_the_daemon_knows_never_enters_the_step_ring() {
    let redactor = Redactor::for_run();
    redactor.set_source("test", ["synthetic-secret-value-0123456789".to_owned()]);
    let output = OutputRegistry::with_redactor(redactor);
    let agent = AgentId::from_parts(1, 1);
    let turn = TurnId::from_parts(2, 1);
    let key = AdoptionKey {
        pid: 10,
        start_time: 20,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.capture_step(
        &key,
        open_event("toolu_1", SESSION, "echo synthetic-secret-value-0123456789"),
    );
    output.confirm(agent, turn, &SessionId::new(SESSION));
    let step = &output.steps_since(agent, 0).unwrap().items[0];
    assert_eq!(step.args[0].value, "echo (redacted)");
}
fn ready_text(output: &OutputRegistry, agent: AgentId, turn: TurnId, id: &str) -> String {
    match output.step_text(agent, turn, id) {
        Fetch::Ready {
            result: Some(result),
            ..
        } => result.text,
        other => panic!("expected a result for {id}, got {other:?}"),
    }
}

#[test]
fn the_result_store_keeps_64_results_and_says_so_for_an_evicted_one() {
    let (output, agent, turn, key) = bound();
    output.confirm(agent, turn, &SessionId::new(SESSION));
    for index in 0..70 {
        let id = format!("toolu_{index}");
        output.capture_step(&key, open_event(&id, SESSION, "ls"));
        output.capture_step(&key, close_with(&id, &format!("out {index}")));
    }
    assert!(matches!(
        output.step_text(agent, turn, "toolu_0"),
        Fetch::Gone
    ));
    assert!(matches!(
        output.step_text(agent, turn, "toolu_5"),
        Fetch::Gone
    ));
    assert_eq!(ready_text(&output, agent, turn, "toolu_6"), "out 6");
    assert_eq!(ready_text(&output, agent, turn, "toolu_69"), "out 69");
    assert_eq!(
        output.steps_since(agent, 0).unwrap().items.len(),
        70,
        "results never push a step out of the ring"
    );
}

#[test]
fn a_result_is_cut_at_8_kib_and_a_cut_secret_is_not_half_kept() {
    let (output, agent, turn, key) = bound();
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(&key, open_event("toolu_1", SESSION, "cat big"));
    output.capture_step(&key, close_with("toolu_1", &"x".repeat(30_000)));
    let Fetch::Ready {
        result: Some(result),
        ..
    } = output.step_text(agent, turn, "toolu_1")
    else {
        panic!("a ready result");
    };
    assert!(result.cut && result.text.len() <= steps::KEPT_BYTES);
    let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let on_the_line = format!("{} {token}", "y".repeat(steps::KEPT_BYTES - 10));
    output.capture_step(&key, open_event("toolu_2", SESSION, "cat key"));
    output.capture_step(&key, close_with("toolu_2", &on_the_line));
    assert!(matches!(
        output.step_text(agent, turn, "toolu_2"),
        Fetch::Withheld(reason) if reason == "the text holds a secret"
    ));
}

#[test]
fn a_secret_learned_after_a_result_was_kept_is_taken_out_at_fetch() {
    let redactor = Redactor::for_run();
    let output = OutputRegistry::with_redactor(redactor.clone());
    let agent = AgentId::from_parts(1, 1);
    let turn = TurnId::from_parts(2, 1);
    let key = AdoptionKey {
        pid: 10,
        start_time: 20,
        boot: None,
    };
    output.replace(agent, turn);
    assert!(output.register(agent, turn, &key));
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(&key, open_event("toolu_1", SESSION, "env"));
    output.capture_step(
        &key,
        close_with("toolu_1", "value late-secret-0123456789 here"),
    );
    redactor.set_source("test", ["late-secret-0123456789".to_owned()]);
    assert_eq!(
        ready_text(&output, agent, turn, "toolu_1"),
        "value (redacted) here"
    );
}

#[test]
fn results_of_sensitive_steps_and_connected_tools_are_never_kept() {
    let (output, agent, turn, key) = bound();
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(
        &key,
        open_event("toolu_1", SESSION, "cat ~/.aws/credentials"),
    );
    output.capture_step(&key, close_with("toolu_1", "PRIVATE-TEXT"));
    let Fetch::Withheld(reason) = output.step_text(agent, turn, "toolu_1") else {
        panic!("a withheld result");
    };
    assert_eq!(reason, "the step names a sensitive path");
    assert_eq!(
        output.state().rings[&agent].steps.results.bytes(),
        0,
        "nothing about it is kept"
    );
    output.capture_step(
        &key,
        StepEvent::Open {
            session: SessionId::new(SESSION),
            step: Step {
                id: "toolu_2".to_owned(),
                parent: None,
                kind: StepKind::Mcp,
                tool: "mcp__fs__read".to_owned(),
                args: Vec::new(),
                edit: None,
                plan: Vec::new(),
            },
        },
    );
    output.capture_step(&key, close_with("toolu_2", "PRIVATE-TEXT"));
    assert!(matches!(
        output.step_text(agent, turn, "toolu_2"),
        Fetch::Withheld(_)
    ));
    assert_eq!(output.state().rings[&agent].steps.results.bytes(), 0);
}

#[test]
fn a_step_id_that_opens_again_keeps_none_of_its_old_text() {
    let (output, agent, turn, key) = bound();
    output.confirm(agent, turn, &SessionId::new(SESSION));
    output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
    output.capture_step(&key, close_with("toolu_1", "old text"));
    assert_eq!(ready_text(&output, agent, turn, "toolu_1"), "old text");
    output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
    assert_eq!(output.state().rings[&agent].steps.results.bytes(), 0);
}

#[test]
fn kept_results_count_inside_the_run_cap() {
    let output = OutputRegistry::new();
    for pid in 0..20_u32 {
        let agent = AgentId::from_parts(1, u128::from(pid) + 1);
        let turn = TurnId::from_parts(2, u128::from(pid) + 1);
        let key = AdoptionKey {
            pid: pid + 10,
            start_time: 20,
            boot: None,
        };
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.confirm(agent, turn, &SessionId::new(SESSION));
        for step in 0..64 {
            let id = format!("toolu_{pid}_{step}");
            output.capture_step(&key, open_event(&id, SESSION, "ls"));
            output.capture_step(&key, close_with(&id, &"z".repeat(steps::KEPT_BYTES)));
        }
    }
    let state = output.state();
    assert!(
        state.step_bytes() + state.text_bytes() <= MAX_BYTES,
        "{} bytes",
        state.step_bytes()
    );
    let kept = state
        .rings
        .values()
        .map(|ring| ring.steps.results.bytes())
        .sum::<usize>();
    assert!(kept > 0 && kept <= MAX_BYTES);
    assert!(
        state
            .rings
            .values()
            .all(|ring| ring.steps.results.bytes() <= crate::step_results::BYTES),
        "one agent's results stay within 512 KiB"
    );
}
