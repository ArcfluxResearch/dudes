//! an agent-authored board status never completes a `BoardState` goal, at the live
//! result boundary or on replay, and a goal the loop already decided `Done` stays completed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dudes_proto::record::Hex32;
use dudes_proto::{
    Actor, Entry, Evidence, ExitStatus, GoalBudget, LoopDecision, Predicate, Record, Spend,
    TurnOutcome,
};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen, journal_with_issued_turn};
use dudes_supervisor::{Command, Input, SpendView, predicate_evidence_eligible};
use dudes_types::{
    AgentId, AgentStatus, CardId, CardStatus, GoalId, Lsn, Provenance, Timestamp, Usd,
};

fn card() -> CardId {
    CardId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAZ").unwrap()
}

fn other_card() -> CardId {
    CardId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").unwrap()
}

fn sibling_id() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAT").unwrap()
}

fn peer() -> Actor {
    Actor::Peer {
        pid: 91,
        uid: Some(501),
    }
}

fn browser() -> Actor {
    Actor::BrowserSession {
        session_digest: Hex32::new([7; 32]),
    }
}

fn board() -> Predicate {
    Predicate::BoardState {
        card: card(),
        status: CardStatus::Done,
    }
}

fn done_by(by: Actor) -> Evidence {
    Evidence::Card {
        card: card(),
        status: CardStatus::Done,
        by,
    }
}

fn command() -> Predicate {
    Predicate::Command {
        argv: vec!["true".to_owned()],
        timeout_s: 60,
    }
}

fn command_evidence() -> Evidence {
    Evidence::Command {
        argv: vec!["true".to_owned()],
        program: Some("/usr/bin/true".to_owned()),
        exit: Some(0),
        output_tail: String::new(),
    }
}

fn at(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(10),
        record,
    }
}

fn done_decision(agent: AgentId) -> Entry {
    at(Record::LoopDecided {
        agent,
        decision: LoopDecision::Done,
        gate_trace: Vec::new(),
        input_digest: String::new(),
    })
}

fn clean_exit(turn: dudes_types::TurnId) -> Input {
    Input::ChildExited {
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
    }
}

fn decisions(harness: &Harness) -> Vec<(AgentId, LoopDecision)> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::LoopDecided {
                agent, decision, ..
            } => Some((*agent, decision.clone())),
            _ => None,
        })
        .collect()
}

fn issued(harness: &Harness) -> usize {
    harness
        .records()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count()
}

fn spawned(harness: &Harness) -> usize {
    harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Spawn { .. }))
        .count()
}

fn decidable(harness: &Harness, agent: AgentId) -> bool {
    harness
        .supervisor
        .world()
        .loop_input(
            agent,
            SpendView::default(),
            dudes_policy::WorkInput::default(),
            None,
            Spend::default(),
        )
        .is_some()
}

fn cached_pass(harness: &Harness, goal: GoalId) -> (bool, Provenance) {
    let cached = harness.supervisor.world().goals[&goal].predicate.unwrap();
    (cached.pass, cached.provenance)
}

/// A journal an older daemon could have written: an agent holding a goal on `predicate`, a
/// hired sibling, and one passing `PredicateResult` resting on `evidence`.
fn legacy_journal(
    predicate: Predicate,
    evidence: Evidence,
    provenance: Provenance,
) -> (Vec<Entry>, AgentId, AgentId, GoalId) {
    let (mut entries, agent, _) = journal_with_issued_turn(None, "legacy task");
    entries.truncate(2);
    let Record::GoalSet { goal, .. } = &mut entries[1].record else {
        panic!("the fixture's second entry is its GoalSet");
    };
    goal.predicate = predicate;
    let goal = goal.id;
    let mut hire = entries[0].clone();
    let Record::AgentHired { id, name, .. } = &mut hire.record else {
        panic!("the fixture's first entry is its AgentHired");
    };
    *id = sibling_id();
    "sibling".clone_into(name);
    entries.push(hire);
    entries.push(at(Record::PredicateResult {
        goal,
        turn: None,
        provenance,
        pass: true,
        evidence,
    }));
    (entries, agent, sibling_id(), goal)
}

fn replay(entries: Vec<Entry>) -> Harness {
    let mut harness = Harness::new();
    harness.journal = entries;
    harness.restart();
    harness
}

/// The board-completion table, with the retirement Todo row: `Peer` and `System` authors of any
/// status but `Claimed` and `Todo` may complete a goal; `Agent` and `BrowserSession` authors never
/// may; evidence naming another card, another status or another kind fails; other predicate kinds
/// are not this check's to judge.
#[test]
fn board_eligibility_is_the_decision_table() {
    let authors = [
        peer(),
        Actor::System,
        Actor::Agent {
            agent: AgentId::NIL,
        },
        Actor::Agent {
            agent: sibling_id(),
        },
        browser(),
    ];
    let expected = |status: CardStatus| {
        let may = !matches!(status, CardStatus::Claimed | CardStatus::Todo);
        [may, may, false, false, false]
    };
    for status in [
        CardStatus::Todo,
        CardStatus::Claimed,
        CardStatus::Doing,
        CardStatus::Review,
        CardStatus::Blocked,
        CardStatus::Done,
    ] {
        let predicate = Predicate::BoardState {
            card: card(),
            status,
        };
        let verdicts = authors.map(|by| {
            predicate_evidence_eligible(
                &predicate,
                &Evidence::Card {
                    card: card(),
                    status,
                    by,
                },
            )
        });
        assert_eq!(verdicts, expected(status), "{status:?}");
    }
    for mismatch in [
        Evidence::Card {
            card: other_card(),
            status: CardStatus::Done,
            by: peer(),
        },
        Evidence::Card {
            card: card(),
            status: CardStatus::Doing,
            by: peer(),
        },
        command_evidence(),
    ] {
        assert!(
            !predicate_evidence_eligible(&board(), &mismatch),
            "{mismatch:?}"
        );
    }
    for predicate in [
        command(),
        Predicate::PathHash {
            path: PathBuf::from("out"),
            blake3: "ab".repeat(32),
        },
        Predicate::AgentVerify {
            prompt: "verify".to_owned(),
            budget: Usd::from_dollars(1),
        },
    ] {
        assert!(predicate_evidence_eligible(
            &predicate,
            &done_by(Actor::Agent {
                agent: sibling_id()
            })
        ));
    }
}

/// The live `PredicateDone` boundary journals an ineligible pass as a failure with `Observed`
/// provenance, and the loop continues. Eligible evidence still reaches `Done`.
#[test]
fn live_board_results_fail_unless_an_operator_or_the_daemon_set_the_status() {
    let cases = |holder: AgentId, sibling: AgentId| {
        [
            (
                "holder's own status",
                board(),
                done_by(Actor::Agent { agent: holder }),
                false,
            ),
            (
                "sibling's status",
                board(),
                done_by(Actor::Agent { agent: sibling }),
                false,
            ),
            ("browser session", board(), done_by(browser()), false),
            (
                "another card",
                board(),
                Evidence::Card {
                    card: other_card(),
                    status: CardStatus::Done,
                    by: peer(),
                },
                false,
            ),
            (
                "another status",
                board(),
                Evidence::Card {
                    card: card(),
                    status: CardStatus::Doing,
                    by: peer(),
                },
                false,
            ),
            ("operator", board(), done_by(peer()), true),
            ("daemon", board(), done_by(Actor::System), true),
            ("command predicate", command(), command_evidence(), true),
        ]
    };
    for index in 0..8 {
        let mut harness = Harness::new();
        let agent = harness.hire();
        let sibling = harness.hire();
        let (label, predicate, evidence, completes) = cases(agent, sibling)[index].clone();
        harness.drive(Input::Command(Command::GoalSet {
            completion_check: None,
            agent,
            text: "complete the card".to_owned(),
            predicate,
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 8,
            browser_goal: None,
            goal_replacement: None,
            baseline: None,
        }));
        let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
        let turn = harness.open_turn(agent).unwrap();
        harness.drive(clean_exit(turn));
        harness.trace.clear();
        harness.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: evidence.clone(),
        });
        let journaled = harness
            .records()
            .find_map(|record| match record {
                Record::PredicateResult {
                    pass,
                    provenance,
                    evidence,
                    ..
                } => Some((*pass, *provenance, evidence.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            journaled,
            (completes, Provenance::Observed, evidence),
            "{label}"
        );
        assert_eq!(
            cached_pass(&harness, goal),
            (completes, Provenance::Observed),
            "{label}"
        );
        let last = decisions(&harness).last().unwrap().1.clone();
        assert_eq!(matches!(last, LoopDecision::Done), completes, "{label}");
        assert_eq!(issued(&harness), usize::from(!completes), "{label}");
    }
}

#[test]
fn a_legacy_sibling_pass_replays_without_done_even_after_reassignment() {
    for reassign in [false, true] {
        let (mut entries, agent, sibling, goal) = legacy_journal(
            board(),
            done_by(Actor::Agent {
                agent: sibling_id(),
            }),
            Provenance::Observed,
        );
        if reassign {
            entries.push(at(Record::GoalReassigned {
                goal,
                from: agent,
                to: sibling,
                reason: None,
            }));
        }
        let holder = if reassign { sibling } else { agent };
        let mut harness = replay(entries);
        assert_eq!(cached_pass(&harness, goal), (false, Provenance::Observed));
        assert_eq!(issued(&harness), 0);
        assert!(harness.trace.iter().any(|seen| matches!(seen,
            Seen::RunPredicate { goal: requested } if *requested == goal
        )));
        harness.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: done_by(Actor::Agent { agent: sibling }),
        });
        assert!(matches!(
            decisions(&harness).as_slice(),
            [(who, LoopDecision::Continue { .. })] if *who == holder
        ));
        assert_eq!(issued(&harness), 1);
        assert!(matches!(
            harness.journal[3].record,
            Record::PredicateResult { pass: true, .. }
        ));
    }
}

/// Other predicate kinds, and board results an operator or the daemon authored, replay exactly
/// as recorded: the pass stands, the provenance is not relabelled, and only `Observed` is `Done`.
#[test]
fn replay_keeps_other_predicates_and_provenance_rules() {
    for (predicate, evidence) in [
        (command(), command_evidence()),
        (
            Predicate::PathHash {
                path: PathBuf::from("out"),
                blake3: "ab".repeat(32),
            },
            Evidence::Hash {
                path: PathBuf::from("out"),
                expected: "ab".repeat(32),
                actual: Some("ab".repeat(32)),
            },
        ),
        (
            Predicate::AgentVerify {
                prompt: "verify".to_owned(),
                budget: Usd::from_dollars(1),
            },
            Evidence::Verdict {
                rationale: "verified".to_owned(),
                usage: None,
            },
        ),
        (board(), done_by(peer())),
        (board(), done_by(Actor::System)),
    ] {
        for provenance in [Provenance::Observed, Provenance::Inferred] {
            let (entries, _, _, goal) =
                legacy_journal(predicate.clone(), evidence.clone(), provenance);
            let mut harness = replay(entries);
            assert_eq!(cached_pass(&harness, goal), (true, provenance));
            if matches!(predicate, Predicate::BoardState { .. }) {
                assert_eq!(issued(&harness), 0);
                assert_eq!(decisions(&harness), []);
                harness.drive(Input::PredicateDone {
                    goal,
                    pass: true,
                    evidence: evidence.clone(),
                });
                assert_eq!(cached_pass(&harness, goal), (true, Provenance::Observed));
                assert_eq!(decisions(&harness).last().unwrap().1, LoopDecision::Done);
                continue;
            }
            let last = decisions(&harness).last().unwrap().1.clone();
            assert_eq!(
                matches!(last, LoopDecision::Done),
                provenance == Provenance::Observed,
                "{predicate:?} {provenance:?}"
            );
        }
    }
}

/// A goal whose `LoopDecided::Done` is journaled stays completed after a restart, although the
/// sibling pass that finished it no longer counts: startup decides nothing and issues nothing,
/// and later wakes do not reopen it, for its holder or a new one. A fresh goal works again.
#[test]
fn a_completed_goal_stays_completed_across_restart_and_wakes_until_a_fresh_goal() {
    for reassign in [false, true] {
        let (mut entries, agent, sibling, goal) = legacy_journal(
            board(),
            done_by(Actor::Agent {
                agent: sibling_id(),
            }),
            Provenance::Observed,
        );
        entries.push(done_decision(agent));
        if reassign {
            entries.push(at(Record::GoalReassigned {
                goal,
                from: agent,
                to: sibling,
                reason: None,
            }));
        }
        let holder = if reassign { sibling } else { agent };
        let mut harness = replay(entries);
        assert_eq!(decisions(&harness), []);
        assert_eq!((issued(&harness), spawned(&harness)), (0, 0));
        assert!(!decidable(&harness, holder));
        if !reassign {
            assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
        }
        harness.drive(Input::Command(Command::Resume {
            agent: holder,
            browser: None,
        }));
        harness.drive(Input::Tick);
        harness.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: done_by(peer()),
        });
        assert_eq!(decisions(&harness), []);
        assert_eq!((issued(&harness), spawned(&harness)), (0, 0));
        harness.set_goal(holder, &["true"]);
        let fresh = harness.supervisor.world().agents[&holder].goal.unwrap();
        assert_ne!(fresh, goal);
        assert_eq!((issued(&harness), spawned(&harness)), (1, 1));
    }
}

/// The latch takes the goal the agent held when `Done` was folded, not a goal that merely has a
/// cached result: the older goal, reassigned elsewhere, is still decided for.
#[test]
fn the_latch_follows_the_goal_held_at_the_done_decision() {
    let (mut entries, agent, sibling, old) = legacy_journal(
        board(),
        done_by(Actor::Agent {
            agent: sibling_id(),
        }),
        Provenance::Observed,
    );
    let mut replacement = entries[1].clone();
    let fresh = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAR").unwrap();
    let Record::GoalSet { goal, .. } = &mut replacement.record else {
        panic!("the fixture's second entry is its GoalSet");
    };
    goal.id = fresh;
    goal.predicate = command();
    entries.push(replacement);
    entries.push(done_decision(agent));
    entries.push(at(Record::GoalReassigned {
        goal: old,
        from: agent,
        to: sibling,
        reason: None,
    }));
    let mut harness = replay(entries);
    assert!(!decidable(&harness, agent));
    assert!(decidable(&harness, sibling));
    assert_eq!(issued(&harness), 0);
    harness.drive(Input::PredicateDone {
        goal: old,
        pass: false,
        evidence: done_by(Actor::Agent { agent: sibling }),
    });
    assert!(matches!(
        decisions(&harness).as_slice(),
        [(who, LoopDecision::Continue { .. })] if *who == sibling
    ));
}

#[test]
fn a_replayed_board_change_invalidates_a_pass_until_fresh_observed_evidence() {
    for by in [peer(), Actor::System] {
        let (mut entries, agent, _, goal) =
            legacy_journal(board(), done_by(by), Provenance::Observed);
        entries.push(at(Record::CardTransition {
            card: card(),
            from: CardStatus::Done,
            to: CardStatus::Doing,
            by: peer(),
            expected_seq: dudes_types::Seq(1),
            reason: None,
        }));
        let mut harness = replay(entries);
        assert!(harness.supervisor.world().goals[&goal].predicate.is_none());
        assert_eq!(issued(&harness), 0);
        assert!(harness.trace.iter().any(|seen| matches!(seen,
            Seen::RunPredicate { goal: requested } if *requested == goal
        )));
        harness.drive(Input::PredicateDone {
            goal,
            pass: false,
            evidence: Evidence::Card {
                card: card(),
                status: CardStatus::Doing,
                by: peer(),
            },
        });
        assert!(matches!(decisions(&harness).as_slice(),
            [(holder, LoopDecision::Continue { .. })] if *holder == agent));
        assert_eq!(cached_pass(&harness, goal), (false, Provenance::Observed));
    }
}

fn set_board_goal(harness: &mut Harness, agent: AgentId, card: CardId) -> GoalId {
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "watch the board".into(),
        predicate: Predicate::BoardState {
            card,
            status: CardStatus::Doing,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(10)),
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    harness.supervisor.world().agents[&agent].goal.unwrap()
}

#[test]
fn retirement_rejudges_held_board_goals_and_preserves_completed_goals() {
    let mut harness = Harness::new();
    let retired = harness.hire();
    let mut watchers = Vec::new();
    for watched in [card(), other_card()] {
        let agent = harness.hire();
        harness.drive(Input::Command(Command::Pause {
            agent,
            browser: None,
        }));
        let goal = set_board_goal(&mut harness, agent, watched);
        harness.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: Evidence::Card {
                card: watched,
                status: CardStatus::Doing,
                by: Actor::System,
            },
        });
        assert_eq!(cached_pass(&harness, goal), (true, Provenance::Observed));
        watchers.push((agent, goal, watched));
    }
    let completed = harness.hire();
    let completed_goal = set_board_goal(&mut harness, completed, card());
    let turn = harness.open_turn(completed).unwrap();
    harness.drive(clean_exit(turn));
    harness.drive(Input::PredicateDone {
        goal: completed_goal,
        pass: true,
        evidence: Evidence::Card {
            card: card(),
            status: CardStatus::Doing,
            by: Actor::System,
        },
    });
    assert_eq!(harness.supervisor.status_of(completed), AgentStatus::Done);
    harness.trace.clear();
    harness.drive(Input::Command(Command::Retire {
        agent: retired,
        reason: "release unfinished work".into(),
        browser_retire: None,
    }));
    for (agent, goal, _) in &watchers {
        assert!(harness.supervisor.world().goals[goal].predicate.is_none());
        assert!(harness.trace.iter().any(|seen| matches!(seen,
            Seen::RunPredicate { goal: judged } if judged == goal)));
        harness.drive(Input::Command(Command::Resume {
            agent: *agent,
            browser: None,
        }));
        assert!(harness.open_turn(*agent).is_none());
    }
    assert_eq!(harness.supervisor.status_of(completed), AgentStatus::Done);
    assert!(!harness.trace.iter().any(|seen| matches!(seen,
        Seen::RunPredicate { goal } if *goal == completed_goal)));
    for (agent, goal, watched) in &watchers {
        harness.drive(Input::PredicateDone {
            goal: *goal,
            pass: *watched == other_card(),
            evidence: Evidence::Card {
                card: *watched,
                status: if *watched == card() {
                    CardStatus::Todo
                } else {
                    CardStatus::Doing
                },
                by: Actor::System,
            },
        });
        assert_eq!(
            harness.supervisor.status_of(*agent),
            if *watched == card() {
                AgentStatus::Running
            } else {
                AgentStatus::Done
            }
        );
    }
    harness.restart();
    assert_eq!(
        harness.supervisor.status_of(watchers[0].0),
        AgentStatus::Running
    );
    assert_eq!(
        harness.supervisor.status_of(watchers[1].0),
        AgentStatus::Done
    );
    assert_eq!(harness.supervisor.status_of(completed), AgentStatus::Done);
}

#[test]
fn a_completed_goal_does_not_cancel_an_admitted_unspawned_turn() {
    let (mut entries, agent, turn) = journal_with_issued_turn(None, "admitted task");
    let Record::TurnIssued { goal, .. } = entries[2].record else {
        panic!("the fixture's third entry is its TurnIssued");
    };
    entries.push(done_decision(agent));
    let mut harness = replay(entries);
    assert!(harness.spawns_of(turn).is_empty());
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    assert_eq!(harness.open_turn(agent), Some(turn));
    let row = &harness.supervisor.world().turns[&turn];
    assert_eq!(row.task, "admitted task");
    assert!(row.answers.is_empty());
    assert_eq!(row.status, dudes_types::TurnStatus::Issued);
    assert_eq!(harness.supervisor.world().goals[&goal].turns_on_goal, 1);
    assert!(matches!(
        harness.supervisor.recovery_opportunity(agent),
        Err(dudes_supervisor::input::RecoveryRefusal::StaleWork)
    ));
    assert_eq!(issued(&harness), 0);
    harness.trace.clear();
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    harness.drive(Input::Tick);
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: command_evidence(),
    });
    assert_eq!((issued(&harness), spawned(&harness)), (0, 0));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    assert_eq!(
        harness.supervisor.world().turns[&turn].task,
        "admitted task"
    );
    assert_eq!(harness.supervisor.world().goals[&goal].turns_on_goal, 1);
    assert_eq!(
        harness
            .supervisor
            .world()
            .completed_goal(agent)
            .map(|(id, _)| id),
        Some(goal)
    );
    assert!(matches!(
        harness.supervisor.recovery_opportunity(agent),
        Err(dudes_supervisor::input::RecoveryRefusal::StaleWork)
    ));
}

/// Invariant 5 across the upgrade: a restart rebuilds the verdict cache the live run decided on,
/// under any clock, and decides nothing the live run did not. The live run meets a sibling's
/// status first, continues, and completes on an operator's.
#[test]
fn replay_rebuilds_the_live_verdicts_and_decides_nothing_new() {
    let mut live = Harness::new();
    let agent = live.hire();
    let sibling = live.hire();
    live.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "complete the card".to_owned(),
        predicate: board(),
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let goal = live.supervisor.world().agents[&agent].goal.unwrap();
    for by in [Actor::Agent { agent: sibling }, peer()] {
        let turn = live.open_turn(agent).unwrap();
        live.drive(clean_exit(turn));
        live.drive(Input::PredicateDone {
            goal,
            pass: true,
            evidence: done_by(by),
        });
    }
    assert!(matches!(
        decisions(&live).as_slice(),
        [
            (_, LoopDecision::Continue { .. }),
            (_, LoopDecision::Continue { .. }),
            (_, LoopDecision::Done)
        ]
    ));
    let verdict = cached_pass(&live, goal);
    assert_eq!(verdict, (true, Provenance::Observed));

    let restarted = replay(live.journal.clone());
    let mut other_clock = Harness::with(
        FakeClock::frozen(1_900_000_000_000),
        FakeHost::wall_clock_anchored(),
    );
    for (entry, position) in live.journal.iter().cloned().zip(1u64..) {
        other_clock.step(Input::Replayed {
            lsn: Lsn(position),
            entry,
        });
    }
    other_clock.drive(Input::Started);
    for rebuilt in [&restarted, &other_clock] {
        assert_eq!(cached_pass(rebuilt, goal), verdict);
        assert_eq!(decisions(rebuilt), []);
        assert_eq!((issued(rebuilt), spawned(rebuilt)), (0, 0));
        assert!(!decidable(rebuilt, agent));
        assert_eq!(rebuilt.supervisor.status_of(agent), AgentStatus::Done);
    }
}
