//! The agent slice is assembled only from replayable journal projections.

use std::collections::{BTreeMap, BTreeSet};

use duded::floor::{AgentSnapshot, snapshot};
use dudes_journal::{Fsync, JournalOptions, Projections, Reader, replay};
use dudes_proto::{
    AgentBudget, AgentView, Entry, Evidence, ExitStatus, Goal, GoalBudget, MailAct, MailFrom,
    MailTo, Predicate, Record, Spend, TurnOutcome,
};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, GoalId, GoalStatus, Lsn, MailId, MailState, Mode,
    Provenance, Provider, SessionId, Timestamp, Tokens, TurnId, Usage, Usd,
};

const AT: i64 = 1_787_394_600_000;

fn entry(offset: i64, record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(AT + offset),
        record,
    }
}

fn hired(id: AgentId, name: &str, role: &str, department: &str) -> Entry {
    entry(
        0,
        Record::AgentHired {
            character: None,
            accent: None,
            id,
            name: name.into(),
            role: role.into(),
            department: department.into(),
            provider: Provider::Claude,
            cwd: "/work/repo".into(),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: Some(Tokens::new(50_000)),
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        },
    )
}

fn mail(id: MailId, agent: AgentId) -> Entry {
    entry(
        0,
        Record::Mail {
            thread: None,
            thread_seq: 0,
            members: Vec::new(),
            id,
            from: MailFrom::Human,
            to: MailTo::Agent { agent },
            act: MailAct::Message,
            subject: "projection sentinel".into(),
            body: None,
            body_len: 0,
            body_path: None,
            body_blake3: None,
            state: MailState::Durable,
            actor: None,
            provenance: None,
        },
    )
}

fn retired(id: AgentId) -> Entry {
    entry(
        0,
        Record::AgentRetired {
            id,
            reason: "set-equality control".into(),
            browser_retire: None,
        },
    )
}

fn daemon_agent_ids(projections: &Projections) -> BTreeSet<AgentId> {
    projections
        .agents()
        .expect("enumerate the daemon projection")
        .into_iter()
        .map(|row| row.id)
        .collect()
}

fn snapshot_agent_ids(snapshot: &AgentSnapshot) -> BTreeSet<AgentId> {
    snapshot.agents.iter().map(|row| row.id).collect()
}

fn same_agent_set(left: &BTreeSet<AgentId>, right: &BTreeSet<AgentId>) -> bool {
    left == right
}

fn apply(projections: &mut Projections, lsn: &mut u64, item: &Entry) {
    assert!(
        projections
            .apply(
                Lsn(*lsn),
                u32::try_from(*lsn).expect("test lsn fits crc"),
                item
            )
            .expect("fold the daemon projection")
    );
    *lsn += 1;
}

fn completed_agent() -> (Harness, AgentId, GoalId) {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(Input::SessionLearned {
        turn,
        session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
    });
    harness.drive(Input::ChildExited {
        final_reply: None,
        turn,
        elapsed_ms: None,
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
            argv: vec!["/usr/bin/true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    (harness, agent, goal)
}

#[test]
fn a_completed_goal_remains_done_after_attach_and_detach() {
    let (mut harness, agent, goal) = completed_agent();
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    harness.drive(Input::Command(Command::Attach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    harness.drive(Input::Command(Command::Detach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: None,
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    assert!(harness.open_turn(agent).is_none());

    let mut projected = Projections::in_memory().unwrap();
    let mut rebuilt = Projections::in_memory().unwrap();
    let mut lsn = 1;
    for item in &harness.journal {
        apply(&mut projected, &mut lsn, item);
    }
    rebuilt
        .apply_batch(harness.journal.iter().enumerate().map(|(index, item)| {
            (
                Lsn(u64::try_from(index).unwrap() + 1),
                u32::try_from(index).unwrap(),
                item,
            )
        }))
        .unwrap();
    for projections in [&mut projected, &mut rebuilt] {
        assert_eq!(
            projections.agent(agent).unwrap().unwrap().status,
            AgentStatus::Done
        );
        let frame = snapshot(projections, Timestamp::from_unix_millis(AT)).unwrap();
        assert_eq!(frame.agents[0].status, AgentStatus::Done);
        assert_eq!(
            frame.agents[0].goal.as_ref().unwrap().status,
            GoalStatus::Achieved
        );
    }
    harness.restart();
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);

    harness.drive(Input::Command(Command::Attach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    harness.drive(Input::Command(Command::GoalClear { agent, goal }));
    harness.drive(Input::Command(Command::Detach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: None,
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Idle);
    for item in &harness.journal[usize::try_from(lsn - 1).unwrap()..] {
        apply(&mut projected, &mut lsn, item);
    }
    assert_eq!(
        projected.agent(agent).unwrap().unwrap().status,
        AgentStatus::Idle
    );
}

#[test]
fn a_completed_goal_waits_for_a_question_after_detach_then_returns_to_done() {
    let (mut harness, agent, _) = completed_agent();
    harness.drive(Input::Command(Command::Attach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
    harness.drive(Input::Command(Command::Ask {
        agent,
        text: "confirm the handoff".into(),
    }));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Attached);
    harness.drive(Input::Command(Command::Detach {
        agent,
        by: dudes_proto::Actor::System,
        provenance: None,
    }));
    for expected in [AgentStatus::Waiting, AgentStatus::Done] {
        assert_eq!(harness.supervisor.status_of(agent), expected);
        let mut projections = Projections::in_memory().unwrap();
        let mut lsn = 1;
        for item in &harness.journal {
            apply(&mut projections, &mut lsn, item);
        }
        assert_eq!(projections.agent(agent).unwrap().unwrap().status, expected);
        assert_eq!(
            snapshot(&mut projections, Timestamp::from_unix_millis(AT))
                .unwrap()
                .agents[0]
                .status,
            expected
        );
        if expected == AgentStatus::Waiting {
            harness.drive(Input::Command(Command::Answer {
                agent,
                text: "confirmed".into(),
                provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
            }));
        }
    }
    harness.restart();
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
}

fn assert_snapshot_matches_daemon(
    projections: &mut Projections,
    today: Timestamp,
) -> BTreeSet<AgentId> {
    let daemon = daemon_agent_ids(projections);
    let snapshot = snapshot(projections, today).expect("build the agent slice");
    let rendered = snapshot_agent_ids(&snapshot);
    assert!(
        same_agent_set(&rendered, &daemon),
        "rendered AgentId set must equal daemon enumeration: rendered={rendered:?} daemon={daemon:?}"
    );
    daemon
}

fn goal(agent: AgentId, id: GoalId, text: &str, hop_cap: u32) -> Entry {
    entry(
        0,
        Record::GoalSet {
            agent,
            goal: Goal {
                completion_check: None,
                id,
                text: text.into(),
                predicate: Predicate::Command {
                    argv: vec!["cargo".into(), "test".into()],
                    timeout_s: 90,
                },
                budget: GoalBudget {
                    usd: Some(Usd::from_dollars(4)),
                    tokens: None,
                },
                hop_cap,
                baseline: None,
            },
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    )
}

fn ended(agent: AgentId, turn: TurnId, goal: GoalId, usage: Usage) -> [Entry; 2] {
    [
        entry(
            0,
            Record::TurnIssued {
                agent,
                turn,
                goal,
                task: "field mapping control".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn::ZERO,
                answers: Vec::new(),
            },
        ),
        entry(
            0,
            Record::TurnEnded {
                final_reply: None,
                elapsed_ms: None,
                session_usage: None,
                agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: Some(usage),
                model_usage: BTreeMap::new(),
                last_message: None,
                session: None,
                outcome: Some(TurnOutcome::Completed),
            },
        ),
    ]
}

fn matches_agent_fields(
    view: &AgentView,
    id: AgentId,
    role: &str,
    status: AgentStatus,
    goal_text: &str,
    spent: Spend,
    question: Option<&str>,
) -> bool {
    view.id == id
        && view.role == role
        && view.status == status
        && view
            .goal
            .as_ref()
            .is_some_and(|goal| goal.text == goal_text && goal.spent == spent)
        && view.spent_today == spent
        && view.question.as_deref() == question
}

#[test]
fn snapshot_fields_equal_agent_projection() {
    let a = AgentId::from_parts(9, 1);
    let b = AgentId::from_parts(9, 2);
    let a_goal = GoalId::from_parts(10, 1);
    let b_goal = GoalId::from_parts(10, 2);
    let a_spend = Spend {
        usd: Usd::from_micros(1_000_000),
        tokens: Tokens::new(8),
        work_tokens: Tokens::new(3),
    };
    let b_spend = Spend {
        usd: Usd::from_micros(2_000_000),
        tokens: Tokens::new(7),
        work_tokens: Tokens::new(7),
    };
    let mut projections = Projections::in_memory().expect("in-memory daemon projection");
    let mut lsn = 1;
    let a_usage = Usage {
        input: Tokens::new(1),
        output: Tokens::new(2),
        cache_read: Tokens::new(5),
        cache_creation: Tokens::new(0),
        usd: Some(a_spend.usd),
    };
    let b_usage = Usage {
        input: Tokens::new(3),
        output: Tokens::new(4),
        cache_read: Tokens::new(0),
        cache_creation: Tokens::new(0),
        usd: Some(b_spend.usd),
    };
    for item in [
        hired(a, "Kim", "ROLE-A", "engineering"),
        hired(b, "Hector", "ROLE-B", "infra"),
        goal(a, a_goal, "GOAL-A", 3),
        goal(b, b_goal, "GOAL-B", 5),
    ] {
        apply(&mut projections, &mut lsn, &item);
    }
    for item in ended(a, TurnId::from_parts(11, 1), a_goal, a_usage) {
        apply(&mut projections, &mut lsn, &item);
    }
    for item in ended(b, TurnId::from_parts(11, 2), b_goal, b_usage) {
        apply(&mut projections, &mut lsn, &item);
    }
    let question = entry(
        0,
        Record::HumanQuestion {
            agent: a,
            text: "QUESTION-A".into(),
        },
    );
    apply(&mut projections, &mut lsn, &question);

    let today = Timestamp::from_unix_millis(AT);
    let snapshot = snapshot(&mut projections, today).expect("build agent snapshot");
    let a_view = snapshot.agents.iter().find(|view| view.id == a).unwrap();
    let b_view = snapshot.agents.iter().find(|view| view.id == b).unwrap();
    assert!(matches_agent_fields(
        a_view,
        a,
        "ROLE-A",
        AgentStatus::Waiting,
        "GOAL-A",
        a_spend,
        Some("QUESTION-A")
    ));
    assert!(matches_agent_fields(
        b_view,
        b,
        "ROLE-B",
        AgentStatus::Idle,
        "GOAL-B",
        b_spend,
        None
    ));

    let mut substituted = a_view.clone();
    substituted.role = b_view.role.clone();
    assert!(
        !matches_agent_fields(
            &substituted,
            a,
            "ROLE-A",
            AgentStatus::Waiting,
            "GOAL-A",
            a_spend,
            Some("QUESTION-A")
        ),
        "a value substituted from B must not satisfy A's projected field mapping"
    );
}

#[test]
fn snapshot_agent_set_equals_world_enumeration() {
    let a = AgentId::from_parts(8, 1);
    let b = AgentId::from_parts(8, 2);
    let retired_agent = AgentId::from_parts(8, 3);
    let added = AgentId::from_parts(8, 4);
    let today = Timestamp::from_unix_millis(AT);
    let mut projections = Projections::in_memory().expect("in-memory daemon projection");
    let mut lsn = 1;

    for item in [
        hired(a, "Kim", "backend engineer", "engineering"),
        hired(b, "Hector", "infra lead", "infra"),
        hired(retired_agent, "Sophia", "accountant", "compliance"),
        retired(retired_agent),
    ] {
        apply(&mut projections, &mut lsn, &item);
    }
    let initial = assert_snapshot_matches_daemon(&mut projections, today);
    assert_eq!(initial, BTreeSet::from([a, b]));
    assert!(
        !initial.contains(&retired_agent),
        "a retired agent is not in the daemon enumeration"
    );

    let add = hired(added, "Desmond", "infra", "infra");
    apply(&mut projections, &mut lsn, &add);
    let after_add = assert_snapshot_matches_daemon(&mut projections, today);
    assert_ne!(after_add, initial, "hiring an agent changes the set");
    assert_eq!(after_add, BTreeSet::from([a, b, added]));

    let retire = retired(b);
    apply(&mut projections, &mut lsn, &retire);
    let after_retire = assert_snapshot_matches_daemon(&mut projections, today);
    assert_ne!(after_retire, after_add, "retiring an agent changes the set");
    assert_eq!(after_retire, BTreeSet::from([a, added]));

    let subset = BTreeSet::from([a]);
    assert!(
        subset.is_subset(&after_retire),
        "control: the malformed rendered set is a proper subset"
    );
    assert!(
        !same_agent_set(&subset, &after_retire),
        "a subset-only comparator would accept a missing daemon agent"
    );
}

#[test]
fn hand_written_fixture_roster_fails_replay_proof() {
    let a = AgentId::from_parts(12, 1);
    let b = AgentId::from_parts(12, 2);
    let seeded = AgentId::from_parts(12, 99);
    let history = [
        hired(a, "Kim", "journal role A", "engineering"),
        hired(b, "Hector", "journal role B", "infra"),
    ];
    let dir = tempfile::tempdir().expect("temporary journal root");
    let journal = dir.path().join("journal");
    let mut live = Projections::open_with(&dir.path().join("live.redb"), Fsync::None)
        .expect("live projections");
    let (mut writer, _) = dudes_journal::open(
        &journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .expect("journal writer");
    for item in &history {
        let (lsn, crc) = writer.append_witnessed(item).expect("append journal row");
        assert!(live.apply(lsn, crc, item).expect("apply live row"));
    }
    drop(writer);

    let today = Timestamp::from_unix_millis(AT);
    let live_snapshot = snapshot(&mut live, today).expect("build live snapshot");
    assert_eq!(
        live_snapshot
            .agents
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [a, b],
        "two rows derive from the journal"
    );
    let mut replayed = Projections::open_with(&dir.path().join("replay.redb"), Fsync::None)
        .expect("fresh projections");
    replay(&Reader::new(&journal), &mut replayed).expect("replay journal");
    let replayed_snapshot = snapshot(&mut replayed, today).expect("build replay snapshot");
    assert_eq!(
        replayed_snapshot, live_snapshot,
        "fresh replay reconstructs both journal-derived rows"
    );

    let mut fixture_roster = live_snapshot.clone();
    let mut fabricated = fixture_roster.agents[0].clone();
    fabricated.id = seeded;
    fabricated.name = "fixture-only".into();
    fixture_roster.agents.push(fabricated);
    assert_ne!(
        fixture_roster, replayed_snapshot,
        "a directly seeded roster is not reconstructed by replay"
    );
    assert!(
        replayed_snapshot.agents.iter().all(|row| row.id != seeded),
        "the seeded AgentView is absent from the replayed journal state"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // one literal assertion per AgentView source plus replay controls
fn snapshot_is_rebuilt_from_journal() {
    let _: fn(&mut Projections, Timestamp) -> anyhow::Result<AgentSnapshot> = snapshot;
    let source = include_str!("../src/floor.rs");
    assert!(
        !source.contains("Result<FloorProjection>"),
        "the projection returns no whole-floor wire value"
    );
    assert!(!source.contains("FloorProjection::default"));
    assert!(!source.contains("spend_by_goal"));
    assert!(!source.contains("spend_by_day"));
    assert_eq!(
        source.matches(".spend_views(").count(),
        1,
        "one source site performs the budget read for every agent"
    );

    let a = AgentId::from_parts(1, 1);
    let b = AgentId::from_parts(1, 2);
    let fake = AgentId::from_parts(1, 99);
    let goal = GoalId::from_parts(2, 1);
    let turn = TurnId::from_parts(3, 1);
    let session = SessionId::new("session-from-journal");
    let unread_mail = MailId::from_parts(4, 1);
    let acked_mail = MailId::from_parts(4, 2);
    let usage = Usage {
        input: Tokens::new(3),
        output: Tokens::new(5),
        cache_read: Tokens::new(2),
        cache_creation: Tokens::new(1),
        usd: Some(Usd::from_micros(1_250_000)),
    };
    let history = vec![
        hired(a, "Kim", "backend engineer", "engineering"),
        hired(b, "Hector", "infra lead", "infra"),
        entry(
            2,
            Record::GoalSet {
                agent: a,
                goal: Goal {
                    completion_check: None,
                    id: goal,
                    text: "ship the journal projection".into(),
                    predicate: Predicate::Command {
                        argv: vec!["cargo".into(), "test".into()],
                        timeout_s: 90,
                    },
                    budget: GoalBudget {
                        usd: Some(Usd::from_dollars(4)),
                        tokens: Some(Tokens::new(20_000)),
                    },
                    hop_cap: 7,
                    baseline: None,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        ),
        entry(
            3,
            Record::TurnIssued {
                agent: a,
                turn,
                goal,
                task: "first projected turn".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn::ZERO,
                answers: Vec::new(),
            },
        ),
        entry(
            4,
            Record::SessionLearned {
                agent: a,
                turn,
                session: session.clone(),
            },
        ),
        entry(
            5,
            Record::TurnEnded {
                final_reply: None,
                elapsed_ms: None,
                session_usage: None,
                agent: a,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: Some(usage),
                model_usage: BTreeMap::new(),
                last_message: Some("not done yet".into()),
                session: Some(session.clone()),
                outcome: Some(TurnOutcome::Completed),
            },
        ),
        entry(
            6,
            Record::PredicateResult {
                goal,
                turn: Some(turn),
                provenance: Provenance::Observed,
                pass: false,
                evidence: Evidence::Command {
                    argv: vec!["cargo".into(), "test".into()],
                    program: Some("/usr/bin/cargo".into()),
                    exit: Some(1),
                    output_tail: "one test still fails".into(),
                },
            },
        ),
        entry(
            7,
            Record::HumanQuestion {
                agent: a,
                text: "QUESTION-NONCE-438".into(),
            },
        ),
        mail(unread_mail, a),
        mail(acked_mail, a),
        entry(
            10,
            Record::MailState {
                id: acked_mail,
                state: MailState::Acked,
            },
        ),
    ];

    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let mut live = Projections::open_with(&dir.path().join("live.redb"), Fsync::None).unwrap();
    let (mut writer, _) = dudes_journal::open(
        &journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let mut final_lsn = Lsn::ZERO;
    let mut predicate_lsn = None;
    for item in &history {
        let (lsn, crc) = writer.append_witnessed(item).unwrap();
        assert!(live.apply(lsn, crc, item).unwrap());
        if matches!(&item.record, Record::PredicateResult { .. }) {
            predicate_lsn = Some(lsn);
        }
        final_lsn = lsn;
    }
    drop(writer);

    let today = Timestamp::from_unix_millis(AT + 5);
    let live_snapshot = snapshot(&mut live, today).unwrap();
    assert_eq!(live_snapshot.lsn, final_lsn);
    assert_eq!(
        live_snapshot
            .agents
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [a, b],
        "the builder preserves the projection's canonical AgentId order"
    );

    let a_view = live_snapshot.agents.iter().find(|row| row.id == a).unwrap();
    assert_eq!(a_view.name, "Kim");
    assert_eq!(a_view.role, "backend engineer");
    assert_eq!(a_view.department, "engineering");
    assert_eq!(a_view.provider, Provider::Claude);
    assert_eq!(a_view.status, AgentStatus::Waiting);
    assert_eq!(a_view.mode, Mode::Managed);
    assert!(!a_view.paused && !a_view.halted);
    assert_eq!(a_view.breaker, BreakerRung::Healthy);
    assert_eq!(a_view.turns, 1);
    assert_eq!(a_view.unread, 1);
    assert_eq!(a_view.question.as_deref(), Some("QUESTION-NONCE-438"));
    assert_eq!(a_view.session.as_ref(), Some(&session));
    assert_eq!(
        a_view.last_turn_ended_at,
        Some(Timestamp::from_unix_millis(AT + 5))
    );
    let charged = Spend {
        usd: Usd::from_micros(1_250_000),
        tokens: Tokens::new(11),
        work_tokens: Tokens::new(9),
    };
    assert_eq!(a_view.spent_today, charged);
    let goal_view = a_view.goal.as_ref().expect("A has its journaled goal");
    assert_eq!(
        (
            goal_view.id,
            goal_view.text.as_str(),
            goal_view.status,
            goal_view.budget,
            goal_view.turns,
            goal_view.hop_cap,
            goal_view.spent,
        ),
        (
            goal,
            "ship the journal projection",
            GoalStatus::InProgress,
            GoalBudget {
                usd: Some(Usd::from_dollars(4)),
                tokens: Some(Tokens::new(20_000)),
            },
            1,
            7,
            charged,
        )
    );
    assert_eq!(
        goal_view.last_result,
        Some(dudes_proto::PredicateOutcome {
            pass: false,
            provenance: Provenance::Observed,
            lsn: predicate_lsn.expect("the predicate result was appended"),
            not_run: false,
        })
    );

    let b_view = live_snapshot.agents.iter().find(|row| row.id == b).unwrap();
    assert_eq!(
        b_view.unread, 0,
        "no-mail is a projected zero, not A's count"
    );
    assert_eq!(b_view.status, AgentStatus::Idle);
    assert_eq!(b_view.goal, None);
    assert_eq!(b_view.spent_today, Spend::default());

    let mut fresh = Projections::open_with(&dir.path().join("fresh.redb"), Fsync::None).unwrap();
    replay(&Reader::new(&journal), &mut fresh).unwrap();
    let rebuilt = snapshot(&mut fresh, today).unwrap();
    assert_eq!(
        rebuilt, live_snapshot,
        "fresh replay rebuilds the same AgentViews"
    );

    let fabricated = AgentView {
        character: None,
        accent: None,
        id: fake,
        name: "fixture-only".into(),
        role: "not journaled".into(),
        look_lsn: None,
        department: "none".into(),
        provider: Provider::Claude,
        workspace: None,
        status: AgentStatus::Done,
        live_activity: None,
        mode: Mode::Managed,
        paused: false,
        halted: false,
        automatic_hold: None,
        breaker: BreakerRung::Healthy,
        goal: None,
        turns: 0,
        spent_today: Spend::default(),
        unread: 99,
        question: None,
        session: None,
        last_turn_ended_at: None,
        worktree: None,
        model: None,
        turn_model: None,
        shares_checkout: false,
        question_lsn: None,
        control_lsn: None,
    };
    let mut poisoned = live_snapshot;
    poisoned.agents.push(fabricated);
    assert_ne!(
        poisoned, rebuilt,
        "a direct row cannot survive journal replay"
    );
    assert!(rebuilt.agents.iter().all(|row| row.id != fake));
}

/// Two agents, two different non-zero costs, and the same two after a fresh replay.
///
/// **Distinctness is the property, and one number does not carry it.** `spent_today` rendering as
/// non-zero proves the field is wired; that two agents render *their own* figures is what the
/// per-agent cost check is about, and what a floor showing one agent's spend against another's name would
/// fail while every single-agent assertion stayed green.
///
/// The `TurnEnded` records here are written by hand, and the division of labour is deliberate:
/// this test pins the **fold and the replay** — production code in
/// `crates/dudes-journal/src/projections/` — while the stub-provider acceptance script pins that a real turn
/// against the credit-free stub produces such a record at all. Neither proves the other, and
/// saying so is what keeps this from reading as an end-to-end claim it does not make.
///
/// Control, measured: deleting the `usage` fold in `crates/dudes-journal/src/projections/turns.rs`
/// reddens this on both snapshots.
#[test]
fn two_agents_keep_their_own_non_zero_costs_through_replay() {
    let a = AgentId::from_parts(31, 1);
    let b = AgentId::from_parts(31, 2);
    // Different in both components, so a fold that dropped either one is still caught.
    let a_usage = Usage {
        usd: Some(Usd::from_micros(1_250_000)),
        input: Tokens::new(11),
        ..Usage::ZERO
    };
    let b_usage = Usage {
        usd: Some(Usd::from_micros(3_400_000)),
        input: Tokens::new(29),
        ..Usage::ZERO
    };

    let mut history = vec![
        hired(a, "Ada", "backend engineer", "engineering"),
        hired(b, "Grace", "frontend engineer", "engineering"),
    ];
    history.extend(ended(
        a,
        TurnId::from_parts(31, 11),
        GoalId::from_parts(31, 21),
        a_usage,
    ));
    history.extend(ended(
        b,
        TurnId::from_parts(31, 12),
        GoalId::from_parts(31, 22),
        b_usage,
    ));

    let dir = tempfile::tempdir().expect("temporary journal root");
    let journal = dir.path().join("journal");
    let mut live = Projections::open_with(&dir.path().join("live.redb"), Fsync::None)
        .expect("live projections");
    let (mut writer, _) = dudes_journal::open(
        &journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .expect("journal writer");
    for item in &history {
        let (lsn, crc) = writer.append_witnessed(item).expect("append journal row");
        assert!(live.apply(lsn, crc, item).expect("apply live row"));
    }
    drop(writer);

    let today = Timestamp::from_unix_millis(AT);
    let live_snapshot = snapshot(&mut live, today).expect("build live snapshot");
    let mut replayed = Projections::open_with(&dir.path().join("replay.redb"), Fsync::None)
        .expect("fresh projections");
    replay(&Reader::new(&journal), &mut replayed).expect("replay journal");
    let replayed_snapshot = snapshot(&mut replayed, today).expect("build replay snapshot");

    for (label, shot) in [("live", &live_snapshot), ("replayed", &replayed_snapshot)] {
        let spend = |id: AgentId| {
            shot.agents
                .iter()
                .find(|row| row.id == id)
                .unwrap_or_else(|| panic!("{label}: no row for that agent"))
                .spent_today
        };
        let (first, second) = (spend(a), spend(b));
        assert_ne!(
            first,
            Spend::default(),
            "{label}: a turn that ended with usage left a zero spend"
        );
        assert_ne!(
            second,
            Spend::default(),
            "{label}: a turn that ended with usage left a zero spend"
        );
        assert_ne!(
            first, second,
            "{label}: two agents charged different usage share one figure, so the floor could \
             show either agent's cost against the other's name"
        );
        assert_eq!(
            Some(first.usd),
            a_usage.usd,
            "{label}: A's charge is A's own"
        );
        assert_eq!(
            Some(second.usd),
            b_usage.usd,
            "{label}: B's charge is B's own"
        );
    }
    assert_eq!(
        replayed_snapshot, live_snapshot,
        "replay rebuilt a different floor than the live fold produced"
    );
}

#[test]
fn workspace_labels_come_from_the_hire_and_survive_fresh_replay() {
    use dudes_proto::record::RecordedWorkspace;

    let ids = [
        AgentId::from_parts(32, 1),
        AgentId::from_parts(32, 2),
        AgentId::from_parts(32, 3),
    ];
    let recorded = [
        Some(RecordedWorkspace {
            id: "alpha".into(),
            label: "Workbench".into(),
        }),
        Some(RecordedWorkspace {
            id: "beta".into(),
            label: "Workbench".into(),
        }),
        None,
    ];
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let (mut writer, _) = dudes_journal::open(
        &journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let mut live = Projections::in_memory().unwrap();
    for (id, workspace) in ids.into_iter().zip(&recorded) {
        let mut item = hired(id, "Ada", "engineer", "engineering");
        if let Record::AgentHired {
            cwd,
            workspace: field,
            ..
        } = &mut item.record
        {
            *cwd = "/private/WORKSPACE-SECRET".into();
            *field = workspace.clone();
        }
        let (lsn, crc) = writer.append_witnessed(&item).unwrap();
        live.apply(lsn, crc, &item).unwrap();
    }
    drop(writer);
    let mut replayed = Projections::in_memory().unwrap();
    replay(&Reader::new(&journal), &mut replayed).unwrap();
    let today = Timestamp::from_unix_millis(AT);
    let expected = snapshot(&mut live, today).unwrap();
    assert_eq!(snapshot(&mut replayed, today).unwrap(), expected);
    assert_eq!(
        expected
            .agents
            .iter()
            .map(|row| row.workspace.clone())
            .collect::<Vec<_>>(),
        recorded
    );
    assert_ne!(expected.agents[0].workspace, expected.agents[1].workspace);
    let wire = serde_json::to_string(&expected.agents).unwrap();
    assert!(!wire.contains("WORKSPACE-SECRET") && !wire.contains("cwd"));
    let legacy_wire = serde_json::to_value(&expected.agents[2]).unwrap();
    assert!(legacy_wire.get("workspace").is_none());
}
