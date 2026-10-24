//! Fixed activity samples from observed hook and managed program boundaries.

use std::collections::BTreeMap;

use dudes_proto::{
    Entry, Event, EventKind, ExitStatus, HookDecision, LiveActivityKind, Record, TurnOutcome,
};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen};
use dudes_supervisor::{Command, HookId, Input};
use dudes_types::{AgentId, Lsn, Mode, Provenance, Timestamp, TurnId};

fn hook(agent: AgentId, kind: EventKind) -> Input {
    Input::Hook {
        hook: HookId(1),
        event: Event {
            agent,
            at: Timestamp::from_unix_millis(1),
            provenance: Provenance::Observed,
            kind,
            raw: Some(serde_json::json!({
                "tool_input": {"command": "private command"},
                "path": "/private/work"
            })),
        },
    }
}

fn pre_tool(agent: AgentId) -> Input {
    hook(
        agent,
        EventKind::PreTool {
            name: "Bash".into(),
            input: serde_json::json!({"command":"private command"}),
        },
    )
}

fn post_tool(agent: AgentId) -> Input {
    hook(
        agent,
        EventKind::PostTool {
            name: "Bash".into(),
            ok: true,
        },
    )
}

fn activity(harness: &Harness) -> Vec<LiveActivityKind> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LiveActivityObserved { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn sampling() -> Harness {
    let mut harness = Harness::new();
    harness.supervisor.enable_activity_sampling();
    harness
}

/// A harness whose clock the test moves, so the spacing and refresh windows are exact.
fn sampling_at(clock: &FakeClock) -> Harness {
    let mut harness = Harness::with(clock.clone(), FakeHost::wall_clock_anchored());
    harness.supervisor.enable_activity_sampling();
    harness
}

#[test]
fn a_hook_reply_waits_on_no_activity_append() {
    let mut harness = sampling();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let before = harness.trace.len();
    harness.drive(pre_tool(agent));
    let trace = &harness.trace[before..];
    assert!(
        trace.iter().any(|seen| matches!(seen, Seen::Reply { .. })),
        "the hook is answered"
    );
    assert!(
        !trace
            .iter()
            .any(|seen| matches!(seen, Seen::Persist { .. })),
        "the reply path appends nothing: {trace:?}"
    );
    assert_eq!(activity(&harness), Vec::<LiveActivityKind>::new());

    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), vec![LiveActivityKind::UsingTool]);
    let encoded = serde_json::to_string(&harness.journal).unwrap();
    assert!(!encoded.contains("private command"));
    assert!(!encoded.contains("/private/work"));
}

#[test]
fn sampling_is_off_until_the_daemon_enables_it() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(pre_tool(agent));
    harness.drive(Input::ActivityObserved {
        turn,
        kind: LiveActivityKind::Compacting,
    });
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), Vec::<LiveActivityKind>::new());
}

#[test]
fn a_steady_kind_is_journaled_once_per_refresh_and_a_change_once_per_spacing() {
    let clock = FakeClock::ticking(1_787_394_600_000, 1);
    let mut harness = sampling_at(&clock);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);

    for _ in 0..=65 {
        harness.drive(post_tool(agent));
        harness.drive(Input::ActivityFlush);
        clock.advance(1_000);
    }
    assert_eq!(
        activity(&harness),
        vec![LiveActivityKind::Working; 3],
        "66 one-second samples of one kind write at 0 s, 30 s and 60 s only"
    );

    harness.drive(pre_tool(agent));
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness).len(), 4, "a change is journaled at once");
    assert_eq!(activity(&harness)[3], LiveActivityKind::UsingTool);

    harness.drive(post_tool(agent));
    for _ in 0..4 {
        harness.drive(Input::ActivityFlush);
        clock.advance(1_000);
    }
    assert_eq!(
        activity(&harness).len(),
        4,
        "a second change within five seconds waits"
    );
    clock.advance(1_000);
    harness.drive(Input::ActivityFlush);
    harness.drive(Input::ActivityFlush);
    assert_eq!(
        activity(&harness)[4..],
        [LiveActivityKind::Working],
        "the held change is journaled once when the spacing allows"
    );
}

#[test]
fn a_lifecycle_record_makes_the_next_sample_a_change() {
    let clock = FakeClock::ticking(1_787_394_600_000, 1);
    let mut harness = sampling_at(&clock);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    harness.drive(post_tool(agent));
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), vec![LiveActivityKind::Working]);

    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    clock.advance(6_000);
    harness.drive(post_tool(agent));
    harness.drive(Input::ActivityFlush);
    assert_eq!(
        activity(&harness),
        vec![LiveActivityKind::Working; 2],
        "the resume cleared the projected kind, so the same kind is a change"
    );
}

#[test]
fn a_backward_clock_step_does_not_hold_a_change() {
    let clock = FakeClock::ticking(1_787_394_600_000, 1);
    let mut harness = sampling_at(&clock);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    harness.drive(post_tool(agent));
    harness.drive(Input::ActivityFlush);
    clock.advance(-3_600_000);
    harness.drive(pre_tool(agent));
    harness.drive(Input::ActivityFlush);
    assert_eq!(
        activity(&harness),
        vec![LiveActivityKind::Working, LiveActivityKind::UsingTool]
    );
}

#[test]
fn refused_and_paused_hooks_leave_no_sample() {
    let mut harness = sampling();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    harness.drive(pre_tool(agent));
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), Vec::<LiveActivityKind>::new());
    assert!(harness.trace.iter().any(|seen| {
        matches!(
            seen,
            Seen::Reply {
                reply: dudes_proto::HookReply {
                    decision: HookDecision::Deny { .. },
                    ..
                },
                ..
            }
        )
    }));

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    harness.drive(hook(agent, EventKind::SessionEnd));
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), vec![LiveActivityKind::Idle]);
}

#[test]
fn managed_samples_require_the_current_open_turn() {
    let mut harness = sampling();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(Input::ActivityObserved {
        turn,
        kind: LiveActivityKind::Compacting,
    });
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), vec![LiveActivityKind::Compacting]);
    let ended = Record::TurnEnded {
        step_digest: None,
        context: None,
        final_reply: None,
        agent,
        turn,
        mode: Mode::Managed,
        provenance: Provenance::Observed,
        elapsed_ms: None,
        exit: ExitStatus::UNKNOWN,
        usage: None,
        session_usage: None,
        model_usage: BTreeMap::new(),
        last_message: None,
        session: None,
        outcome: Some(TurnOutcome::Completed),
    };
    harness.drive(Input::Replayed {
        lsn: Lsn(999),
        entry: Entry {
            at: Timestamp::from_unix_millis(2),
            record: ended,
        },
    });
    harness.drive(Input::ActivityObserved {
        turn,
        kind: LiveActivityKind::UsingTool,
    });
    harness.drive(Input::ActivityObserved {
        turn: TurnId::NIL,
        kind: LiveActivityKind::UsingTool,
    });
    harness.drive(Input::ActivityFlush);
    assert_eq!(activity(&harness), vec![LiveActivityKind::Compacting]);
}
