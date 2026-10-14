#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use duded::settings::{FloorSettings, RunOptions};
use dudes_proto::{LoopDecision, MAIL_INSTRUCTION, MEMORY_INSTRUCTION, NoteAdmission, Record};
use dudes_supervisor::{Input, World};
use dudes_types::{AgentId, GoalId, Lsn, Mode, Provenance, TurnId, Usage};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, floor_post, floor_session, seams};

struct Completed {
    agent: AgentId,
    goal: GoalId,
    turn: TurnId,
    completion_lsn: Lsn,
}

fn start(fixture: &Fixture, host: &SharedHost) -> (duded::Settings, common::Running, SocketAddr) {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v3"
        )),
        port: Some(0),
    });
    let seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::passes()])),
        Arc::new(TestClock::new()),
        duded::startup::Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = duded::boot_with_options(&settings, seams, RunOptions::default())
        .expect("boot the owned gateway fixture");
    let addr = daemon.floor_addr().expect("loopback Floor listener");
    assert!(addr.ip().is_loopback());
    (settings, common::start(daemon), addr)
}

fn end_turn(fixture: &Fixture, host: &SharedHost, turn: TurnId) {
    common::end_turn_with_native_payload(
        fixture,
        host,
        Input::ChildExited {
            final_reply: None,
            elapsed_ms: Some(6_000),
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(Usage::ZERO)),
            model_usage: BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
}

fn complete_goal(fixture: &Fixture, running: &common::Running, host: &SharedHost) -> Completed {
    let agent = common::hire(&running.handle, fixture.state.path());
    let mut params = common::goal_params(agent, &[common::true_program()]);
    params.hop_cap = 1;
    let goal = common::set_goal(&running.handle, params);
    let turn = fixture.await_turn();
    fixture.await_record("ordinary spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
            .then_some(())
    });
    end_turn(fixture, host, turn);
    fixture.await_record("ordinary Done", |record| {
        matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(())
    });
    let entries = fixture.entries();
    assert!(entries.iter().any(|(_, entry)| matches!(&entry.record,
        Record::TurnEnded { turn: ended, elapsed_ms: Some(6_000), usage: Some(Usage::ZERO), provenance: Provenance::Observed, .. } if *ended == turn
    )));
    assert!(entries.iter().any(|(_, entry)| matches!(&entry.record,
        Record::PredicateResult { goal: checked, turn: Some(checked_turn), pass: true, provenance: Provenance::Observed, .. } if *checked == goal && *checked_turn == turn
    )));
    let completion_lsn = entries
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::LoopDecided {
                    decision: LoopDecision::Done,
                    ..
                }
            )
            .then_some(*lsn)
        })
        .expect("ordinary completion identity");
    Completed {
        agent,
        goal,
        turn,
        completion_lsn,
    }
}

fn taught_issue(fixture: &Fixture, completed: &Completed, note_lsn: Lsn, note: &str) -> TurnId {
    let issue = fixture.await_record("taught completed-note issue", |record| {
        matches!(record, Record::TaughtNoteOnlyTurnIssued { note_lsn: queued, .. } if *queued == note_lsn).then(|| record.clone())
    });
    let Record::TaughtNoteOnlyTurnIssued {
        agent,
        turn,
        goal,
        task,
        mode,
        answers,
        lsn_of_decision,
        completion_lsn,
        ..
    } = issue
    else {
        unreachable!()
    };
    assert_eq!(agent, completed.agent);
    assert_eq!(goal, completed.goal);
    assert_eq!(completion_lsn, completed.completion_lsn);
    assert_eq!(mode, Mode::Managed);
    assert!(answers.is_empty());
    assert_eq!(task.len(), 4_528);
    assert_eq!(
        task,
        format!("{note}{MEMORY_INSTRUCTION}{MAIL_INSTRUCTION}")
    );
    let entries = fixture.entries();
    assert!(entries.iter().any(|(lsn, entry)| *lsn == note_lsn && matches!(&entry.record,
        Record::BrowserNoteQueued { agent, text, destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal }, .. }
            if *agent == completed.agent && text == note && text.len() == 4_096 && *goal == completed.goal
    )));
    assert!(entries.iter().any(|(lsn, entry)| *lsn == lsn_of_decision && matches!(&entry.record,
        Record::TaughtNoteTurnDecided { agent, goal, note_lsn: queued, completion_lsn, decision: NoteAdmission::Issue { task: decided_task }, .. }
            if *agent == completed.agent && *goal == completed.goal && *queued == note_lsn
                && *completion_lsn == completed.completion_lsn && decided_task == &task
    )));
    assert!(note_lsn < lsn_of_decision);
    let issue_lsn = entries
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record,
                Record::TaughtNoteOnlyTurnIssued { turn: issued, .. } if issued == turn
            )
            .then_some(*lsn)
        })
        .unwrap();
    assert!(lsn_of_decision < issue_lsn);
    turn
}

fn assert_completed_history(fixture: &Fixture, completed: &Completed, note_turn: TurnId) {
    let entries = fixture.entries();
    let mut world = World::default();
    for (lsn, entry) in &entries {
        world.apply(*lsn, entry);
    }
    assert_eq!(
        world.completed_goal(completed.agent),
        Some((completed.goal, completed.completion_lsn))
    );
    assert_eq!(world.goals[&completed.goal].turns_on_goal, 2);
    assert_eq!(world.turns.len(), 2);
    assert!(world.turns.contains_key(&completed.turn));
    assert!(world.turns.contains_key(&note_turn));
    assert!(world.agents[&completed.agent].open_turn.is_none());
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry.record,
                Record::LoopDecided {
                    decision: LoopDecision::Done,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::BrowserNoteQueued { .. }))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::TaughtNoteOnlyTurnIssued { .. }))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::TurnSpawned { .. }))
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::TurnEnded { .. }))
            .count(),
        2
    );
}

#[test]
fn completed_note_exact_utf8_boundary_is_issued_and_survives_restart() {
    let fixture = Fixture::new();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let (settings, mut running, addr) = start(&fixture, &host);
    let completed = complete_goal(&fixture, &running, &host);
    let (cookie, csrf) = floor_session(&settings, addr);
    let note = "é".repeat(2_048);
    assert_eq!(note.len(), 4_096);
    let initial_launches = host.spawned().len();
    assert_eq!(initial_launches, 1);
    let (head, receipt) = floor_post(
        addr,
        "/floor/chat/send",
        (&cookie, &csrf),
        &json!({
            "request_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": completed.agent,
            "text": note,
        }),
    );
    assert_eq!(
        head.split_whitespace().nth(1),
        Some("200"),
        "4096 UTF-8 bytes are admitted through the authenticated gateway"
    );
    assert_eq!(receipt["state"], "queued");
    let note_lsn = Lsn(receipt["note_lsn"].as_str().unwrap().parse().unwrap());
    let note_turn = taught_issue(&fixture, &completed, note_lsn, &note);
    fixture.await_record("taught note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == note_turn).then_some(())
    });
    let launches = host.spawned();
    assert_eq!(launches.len() - initial_launches, 1);
    let expected_task = format!("{note}{MEMORY_INSTRUCTION}{MAIL_INSTRUCTION}");
    assert_eq!(
        launches[1]
            .argv
            .iter()
            .filter(|arg| *arg == expected_task.as_str())
            .count(),
        1
    );
    let over_limit = format!("{note}x");
    assert_eq!(over_limit.len(), 4_097);
    let (head, _) = floor_post(
        addr,
        "/floor/chat/send",
        (&cookie, &csrf),
        &json!({
            "request_id": "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": completed.agent,
            "text": over_limit,
        }),
    );
    assert_eq!(head.split_whitespace().nth(1), Some("400"));
    end_turn(&fixture, &host, note_turn);
    fixture.await_record("taught note end", |record| {
        matches!(record, Record::TurnEnded { turn, .. } if *turn == note_turn).then_some(())
    });
    running.stop(duded::ExitCause::Signal);
    assert_completed_history(&fixture, &completed, note_turn);
    assert_eq!(host.spawned().len(), 2);
    let restart_host = SharedHost::with_native_evidence(fixture.journal_dir());
    let (_, mut restarted, _) = start(&fixture, &restart_host);
    restarted.stop(duded::ExitCause::Signal);
    assert_completed_history(&fixture, &completed, note_turn);
    assert!(restart_host.spawned().is_empty());
}
