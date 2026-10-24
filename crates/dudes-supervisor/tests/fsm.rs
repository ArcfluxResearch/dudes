//! The agent state machine, driven through the whole machine rather than
//! through `agent_fsm::next` alone.
//!
//! The unit tests next to the transition function cover the edges; these cover the claim that
//! matters — that the machine reaches those states by journaling records, so a replay of the same
//! records reaches the same states.

use std::collections::BTreeMap;

use dudes_proto::goal::Evidence;
use dudes_proto::{
    Actor, Entry, Event, EventKind, ExitStatus, HoldReason, LoopDecision, PeerProvenance, Record,
    TurnOutcome,
};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{ClientId, Command, HookId, Input};
use dudes_types::{AgentId, AgentStatus, Lsn, Provenance, SessionId, Timestamp, TurnId};

fn prime_attached_session(harness: &mut Harness, agent: AgentId) {
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
}

/// The pre-tool event at which a steer is never delivered.
fn pre_tool() -> EventKind {
    EventKind::PreTool {
        name: "Bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
    }
}

/// Drive one hook event to quiescence and return the context its reply carried.
///
/// The `expect` is half the assertion: a steer-carrying reply is parked behind
/// an fsync, so a reply that never arrives is a hung hook rather than an empty one, and the two
/// must not read alike.
fn reply_to(
    harness: &mut Harness,
    agent: AgentId,
    hook: HookId,
    kind: EventKind,
) -> Option<String> {
    harness.drive(Input::Hook {
        hook,
        event: Event {
            agent,
            at: Timestamp::from_unix_millis(1),
            provenance: Provenance::Observed,
            kind,
            raw: None,
        },
    });
    let answered = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Reply { hook: id, reply } if *id == hook => Some(reply),
            _ => None,
        })
        .expect("the hook is answered");
    answered.context.clone()
}

fn attach(agent: AgentId) -> Command {
    Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }
}

fn last_decision(harness: &Harness) -> Option<LoopDecision> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LoopDecided { decision, .. } => Some(decision.clone()),
            _ => None,
        })
        .last()
}

/// The last decision recorded **for one agent**.
///
/// [`last_decision`] reads the last one written by anybody, which is enough while a harness holds
/// one agent and says nothing at all once it holds two: the newest record then belongs to whoever
/// was decided about most recently, not to the agent under test.
fn last_decision_for(harness: &Harness, agent: dudes_types::AgentId) -> Option<LoopDecision> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LoopDecided {
                agent: decided,
                decision,
                ..
            } if *decided == agent => Some(decision.clone()),
            _ => None,
        })
        .last()
}

/// End `turn` and judge `goal` against a failing predicate, which is what releases a decision.
fn end_turn_and_judge(harness: &mut Harness, turn: dudes_types::TurnId, goal: dudes_types::GoalId) {
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
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });
}

/// The agent FSM matches its design — `Idle`, `Running`, `Waiting`, `Done`, `Exhausted`,
/// `Attached` — and each state is reached by the record that means it.
#[test]
fn agent_fsm_matches_section_5() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    assert_eq!(
        harness.supervisor.status_of(agent),
        AgentStatus::Idle,
        "hired, no goal, nothing decided"
    );

    // Idle -> Running: `goal.set` decides `Continue` and the `TurnIssued` puts a turn in flight.
    let goal = harness.set_goal(agent, &["npm", "test"]);
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Running);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // Running -> Idle: an Observed turn end, and the loop re-decides.
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
        last_message: None,
    });
    // The predicate has not answered yet, so the agent is between turns.
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Idle);

    // Idle -> Done: an Observed *passing* predicate result, and only that (invariant 3).
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    assert!(matches!(last_decision(&harness), Some(LoopDecision::Done)));

    // any -> Attached -> Idle: the keyboard, taken and given back. The goal is cleared first so
    // the re-decide after the detach has nothing to say — the edge under test is `Detached`, not
    // what the loop concludes next.
    harness.drive(Input::Command(Command::GoalClear { agent, goal }));
    prime_attached_session(&mut harness, agent);
    harness.drive(Input::Command(attach(agent)));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    assert!(
        harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::OpenPty { .. })),
        "the PTY is opened after the Attached is durable"
    );
    harness.drive(Input::Command(Command::Detach {
        agent,
        by: Actor::System,
        provenance: None,
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Idle);
    assert!(
        !harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::ClosePty { .. })),
        "a confirmed detach cannot ask the host to close twice"
    );
}

/// `Waiting` and the way out of it: an agent that asks a human is held at gate 3 until
/// `agent.answer`, and the question reaches every subscriber.
///
/// **Driven by `Command::Ask`.** No `Input::Hook` carrying `EventKind::HumanQuestion` produces
/// the record; `Command::Ask` is its only producer. This is gate 3's only end-to-end check.
#[test]
fn a_human_question_waits_and_an_answer_releases() {
    let mut harness = Harness::new();
    harness.drive(Input::Command(Command::Subscribe {
        client: ClientId(1),
    }));
    let agent = harness.hire();
    let goal = harness.set_goal(agent, &["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    harness.drive(Input::Command(Command::Ask {
        agent,
        text: "which branch?".into(),
    }));
    assert!(
        harness.trace.iter().any(|seen| matches!(
            seen,
            Seen::Notify { client, method } if *client == ClientId(1) && method == "agent.question"
        )),
        "the subscriber was told"
    );

    // The turn is still in flight, so nothing decides yet; the end releases the decision.
    end_turn_and_judge(&mut harness, turn, goal);
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Waiting);
    assert!(matches!(
        last_decision(&harness),
        Some(LoopDecision::Wait { .. })
    ));

    harness.drive(Input::Command(Command::Answer {
        agent,
        text: "main".into(),
        provenance: Some(PeerProvenance::NoRecordedAgent),
    }));
    assert_eq!(
        harness.supervisor.status_of(agent),
        AgentStatus::Running,
        "answered, decided again, and back at work"
    );
    assert!(harness.records().any(|record| matches!(
        record,
        Record::HumanAnswer {
            provenance: Some(PeerProvenance::NoRecordedAgent),
            ..
        }
    )));
}

/// A durable steer reaches the next **admitted** event and no other, with the connection's exact
/// provenance retained for replay (invariants 1 and 8).
///
/// A steer is admitted at exactly `PromptSubmit` and `PostTool`. This test's predecessor drove
/// `EventKind::PreTool` and asserted the note rode along, which is the behaviour that was removed:
/// a note that rides every event is re-delivered for the life of the agent.
///
/// The pre-tool arm asserts the note is **kept**, not merely withheld. A fix that dropped the
/// note at an inadmissible event would satisfy "no context here" and silently lose the
/// operator's correction, so the second arm is what separates withholding from discarding.
#[test]
fn a_steer_is_withheld_from_an_inadmissible_event_and_consumed_by_an_admitted_one() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(Command::Steer {
        agent,
        text: "inspect the lock ordering".into(),
        provenance: Some(PeerProvenance::NoRecordedAgent),
    }));

    assert!(harness.records().any(|record| matches!(
        record,
        Record::Steer {
            provenance: Some(PeerProvenance::NoRecordedAgent),
            ..
        }
    )));

    assert_eq!(
        reply_to(&mut harness, agent, HookId(91), pre_tool()),
        None,
        "a pre-tool event is not a correction point"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(92), EventKind::PromptSubmit),
        Some("inspect the lock ordering".to_owned()),
        "the note the pre-tool event withheld is still pending, and lands at the next prompt"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(93), EventKind::PromptSubmit),
        None,
        "one delivery consumes it: a second prompt carries nothing"
    );
}

/// A question holds **the agent that asked it**, and nobody else: A goes to `Waiting` while B goes
/// on issuing turns.
///
/// **This is the assertion `cargo nextest run -p dudes-supervisor human_question` names**, and it
/// is a different claim from the test above. That one drives one agent, so *"gate 3 held it"* and
/// *"gate 3 held everybody"* read the same; a `Then::Question` that decided for every agent, or a
/// `Command::Ask` arm that ignored its `agent` field, would leave it green. B is the control that
/// separates them, and B is asserted to be **issuing before** the ask as well as after — otherwise
/// "B still works" would be satisfied by a B that had never worked.
///
/// The token half is not testable here and is not faked: there is no token
/// in this crate. The seam resolves it and the machine receives a `Command::Ask` already carrying
/// the agent — that the *handler* fills that field from `PeerRole::Agent` and not from params is
/// `dudes-rpc`'s `the_asker_is_the_resolved_peer_and_not_anything_in_the_params`, and that a real
/// token over a real socket reaches it is `bins/duded/tests/ask.rs`.
#[test]
fn a_human_question_holds_only_the_asking_agent() {
    let mut harness = Harness::new();
    let (a, goal_a) = harness.hire_with_goal(&["npm", "test"]);
    let (b, goal_b) = harness.hire_with_goal(&["npm", "test"]);
    let turn_a = harness.open_turn(a).expect("A was issued a turn");
    let turn_b = harness.open_turn(b).expect("B was issued a turn");
    assert_ne!(a, b, "control: two agents, not one read twice");
    assert_eq!(
        harness.supervisor.status_of(a),
        AgentStatus::Running,
        "control: A is at work before it asks, so `Waiting` below is a change of state"
    );

    harness.drive(Input::Command(Command::Ask {
        agent: a,
        text: "which branch?".into(),
    }));
    end_turn_and_judge(&mut harness, turn_a, goal_a);

    assert_eq!(
        harness.supervisor.status_of(a),
        AgentStatus::Waiting,
        "the agent that asked is held at gate 3"
    );
    assert!(
        matches!(
            last_decision_for(&harness, a),
            Some(LoopDecision::Wait { .. })
        ),
        "and the reason recorded for A is the human gate: {:?}",
        last_decision_for(&harness, a)
    );
    assert!(
        harness.open_turn(a).is_none(),
        "a held agent has no turn in flight"
    );

    // B's turn ends the same way. Nothing about A's question is B's business, so B re-decides and
    // is issued another turn.
    end_turn_and_judge(&mut harness, turn_b, goal_b);
    assert_eq!(
        harness.supervisor.status_of(b),
        AgentStatus::Running,
        "B keeps issuing: one agent's open question is not a floor-wide hold"
    );
    let next_b = harness.open_turn(b).expect("B was issued another turn");
    assert_ne!(next_b, turn_b, "and it is a new turn, not the old one");
    assert!(
        !matches!(
            last_decision_for(&harness, b),
            Some(LoopDecision::Wait { .. })
        ),
        "B was never held: {:?}",
        last_decision_for(&harness, b)
    );
    assert_eq!(
        harness.supervisor.status_of(a),
        AgentStatus::Waiting,
        "and A is still held after B moved, so the two agents are decided about independently"
    );
}

/// Gate 1 in the machine: `loop.halt` holds the loop *and* ends the running turn, and the end the
/// journal records is `Killed{by: Halt}` — the ledger's reason, not whatever a signalled process
/// reported. This is what makes `TurnStatus::Killed` reachable.
#[test]
fn a_killed_turn_ends_with_outcome_killed_by_the_ledgers_reason() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert!(
        harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::Kill { turn: t, .. } if *t == turn)),
        "the halt reached the child"
    );

    // The child reports whatever it reports; the supervisor's own ledger wins.
    let goal = harness.supervisor.world().agents[&agent]
        .goal
        .expect("the goal stands");
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: None,
            signal: Some(15),
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Failed {
            reason: "killed".into(),
        },
        last_message: None,
    });
    let outcome = harness
        .records()
        .find_map(|record| match record {
            Record::TurnEnded {
                turn: t, outcome, ..
            } if *t == turn => outcome.clone(),
            _ => None,
        })
        .expect("the turn ended");
    assert!(
        matches!(
            outcome,
            TurnOutcome::Killed {
                by: dudes_proto::KillReason::Halt
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        dudes_types::TurnStatus::Killed
    );

    // And gate 1 holds afterwards. The predicate still runs and still passes — the daemon ran
    // it, so its result is `Observed` — but control is consulted before done, so the verdict is
    // `Hold`, not `Done`. That ordering is invariant 5's, and this is where it bites.
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    assert!(
        matches!(
            last_decision(&harness),
            Some(LoopDecision::Hold {
                reason: HoldReason::Halted
            })
        ),
        "control is gate 1 and done is gate 4: {:?}",
        last_decision(&harness)
    );
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Idle);
}

/// `loop.pause` stops the issuing and nothing else, so hooks and mail keep working and it ends
/// no turn. Stated as a test because the difference from `loop.halt` is one line of code and one
/// sentence of contract.
#[test]
fn pause_holds_the_loop_and_kills_nothing() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    harness.trace.clear();

    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    assert!(
        harness
            .trace
            .iter()
            .all(|seen| !matches!(seen, Seen::Kill { .. })),
        "a pause signals nothing"
    );
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Running);
}

/// A restart replays every record and reaches the same statuses, because every status edge is
/// driven by a record and `Input::Replayed` folds through the same function as `Input::Persisted`
/// (invariant 1).
#[test]
fn replay_reaches_the_same_statuses() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
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
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    let before = harness.supervisor.status_of(agent);
    assert_eq!(before, AgentStatus::Done);

    harness.restart();
    assert_eq!(
        harness.supervisor.status_of(agent),
        before,
        "the same records, the same state"
    );
}

/// A replayed attachment has no independently confirmed old-PTY absence. The machine holds it
/// rather than inventing `Detached` and resuming managed work after a daemon crash.
#[test]
fn a_replayed_attachment_stays_held_without_a_confirmed_close() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    prime_attached_session(&mut harness, agent);
    harness.drive(Input::Command(attach(agent)));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    harness.set_goal(agent, &["npm", "test"]);

    let before = harness.records().count();
    harness.trace.clear();
    harness.restart();

    let after: Vec<_> = harness.records().skip(before).collect();
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    assert!(
        !after.iter().any(|record| matches!(record,
            Record::Detached { agent: who, .. }
            | Record::LoopDecided { agent: who, .. }
            | Record::TurnIssued { agent: who, .. }
            if *who == agent
        )),
        "replay must not detach, decide or issue a turn for an unconfirmed PTY: {after:?}"
    );
    assert!(
        !harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::ClosePty { .. } | Seen::Spawn { .. })),
        "the new run cannot act on an old PTY it cannot identify"
    );
}
