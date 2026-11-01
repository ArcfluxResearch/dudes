//! A goal turn of a moved agent that crashes waits for the operator's consent like any other,
//! and its re-issue runs in the place the agent moved to.

use std::path::{Path, PathBuf};

use dudes_proto::goal::Evidence;
use dudes_proto::placement::AgentMove;
use dudes_proto::record::{Hex32, RecordedWorkspace};
use dudes_proto::recovery::{RecoveryAttribution, RecoveryReason};
use dudes_proto::{AgentBudget, Entry, LoopDecision, Record};
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand, RecoveryRefusal};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input, ProbeAnswer, RepositoryAdmission};
use dudes_types::{AgentId, GoalId, Provider, SessionId, Timestamp, TurnId, Usd};

fn workspace(id: &str) -> RecordedWorkspace {
    RecordedWorkspace {
        id: id.into(),
        label: id.into(),
    }
}

fn hire(harness: &mut Harness) -> AgentId {
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/repo/service"),
        workspace: Some(workspace("service")),
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
    }));
    let agent = *harness
        .supervisor
        .world()
        .agents
        .keys()
        .next_back()
        .expect("the hire was journaled");
    admit(harness, agent);
    agent
}

/// The runtime admits a place's repositories before a turn may start there; the harness stands in.
fn admit(harness: &mut Harness, agent: AgentId) {
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(RepositoryAdmission::new([(agent, hired, true)]));
}

fn append(harness: &mut Harness, record: Record) {
    harness.append_replayed(Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    });
}

fn move_to(harness: &mut Harness, agent: AgentId, project: &str, cwd: &str) {
    append(
        harness,
        Record::AgentMoved {
            agent,
            to: AgentMove {
                cwd: cwd.into(),
                workspace: workspace(project),
                worktree: None,
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(10)),
                    tokens_per_day: None,
                },
            },
        },
    );
    admit(harness, agent);
}

fn learn(harness: &mut Harness, agent: AgentId, session: &str) {
    append(
        harness,
        Record::SessionLearned {
            agent,
            turn: TurnId::from_bytes([2; 16]),
            session: SessionId::new(session),
        },
    );
}

fn spawns(harness: &Harness) -> Vec<(PathBuf, Vec<String>)> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some((
                spec.cwd.clone(),
                spec.argv
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn issued(harness: &Harness) -> usize {
    harness
        .journal
        .iter()
        .filter(|entry| matches!(entry.record, Record::TurnIssued { .. }))
        .count()
}

/// The goal turn is running when the daemon dies and the child is gone when it comes back. The
/// predicate then fails, so the only way forward is the operator's consent.
fn crash_then_fail_the_predicate(
    harness: &mut Harness,
    agent: AgentId,
    goal: GoalId,
    turn: TurnId,
) {
    let pid = harness.supervisor.world().turns[&turn]
        .pid
        .expect("the child's pid was recorded");
    harness.host.answer(pid, ProbeAnswer::NoSuchPid);
    harness.trace.clear();
    harness.restart();
    admit(harness, agent);
    harness.trace.clear();
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

fn consent(harness: &mut Harness, agent: AgentId, choice: RecoveryChoice) -> RecoveryCommand {
    RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected: harness
            .supervisor
            .recovery_opportunity(agent)
            .expect("the crashed turn is owed a decision")
            .expected,
        choice,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    }
}

fn no_work_without_consent(harness: &Harness) {
    assert!(spawns(harness).is_empty(), "nothing spawns");
    assert!(
        harness.trace.iter().all(|seen| !matches!(
            seen,
            Seen::Persist {
                entry: Entry {
                    record: Record::TurnIssued { .. }
                        | Record::LoopDecided {
                            decision: LoopDecision::Continue { .. },
                            ..
                        },
                    ..
                },
                ..
            }
        )),
        "no further turn is decided"
    );
}

/// Hire in one project, learn a session there, move away and learn another, optionally move back.
fn moved_agent(harness: &mut Harness, back: bool) -> AgentId {
    let agent = hire(harness);
    learn(harness, agent, "service-session");
    move_to(harness, agent, "billing", "/repo/billing");
    if back {
        learn(harness, agent, "billing-session");
        move_to(harness, agent, "service", "/repo/service");
    }
    agent
}

fn assert_recovery_runs_in(
    back: bool,
    choice: RecoveryChoice,
    cwd: &str,
    resumes: Option<&str>,
    never: &str,
) {
    let mut harness = Harness::new();
    let agent = moved_agent(&mut harness, back);
    let goal = harness.set_goal(agent, &["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert_eq!(spawns(&harness)[0].0, Path::new(cwd), "the first run");
    crash_then_fail_the_predicate(&mut harness, agent, goal, turn);

    assert!(harness.records().any(|record| matches!(record,
        Record::RecoveryRequired(required)
            if required.binding.work.turn == turn && required.reason == RecoveryReason::Vanished
    )));
    no_work_without_consent(&harness);
    assert_eq!(issued(&harness), 1, "the crashed turn is the only one");

    let command = consent(&mut harness, agent, choice);
    harness.trace.clear();
    let handled = harness.recover(command);
    assert_eq!(handled.recovery().expect("progress").refusal, None);
    let reissued = spawns(&harness);
    assert_eq!(reissued.len(), 1, "one process for the consented run");
    let (folder, argv) = &reissued[0];
    assert_eq!(folder, Path::new(cwd));
    let line = argv.join(" ");
    assert!(!line.contains(never), "{line}");
    assert!(
        resumes.is_none_or(|session| line.contains(session)),
        "{line}"
    );
    assert_eq!(issued(&harness), 2, "one new turn, issued by the consent");
}

#[test]
fn a_moved_agents_crashed_turn_waits_for_consent_and_continues_in_its_new_project() {
    assert_recovery_runs_in(
        false,
        RecoveryChoice::FreshWithRecap,
        "/repo/billing",
        None,
        "service-session",
    );
}

#[test]
fn a_returned_agents_crashed_turn_continues_in_its_old_place_on_its_old_session() {
    assert_recovery_runs_in(
        true,
        RecoveryChoice::TryRecorded,
        "/repo/service",
        Some("service-session"),
        "billing-session",
    );
}

#[test]
fn a_turn_issued_before_a_move_is_refused_and_admits_no_more_work() {
    let mut harness = Harness::new();
    let agent = hire(&mut harness);
    let goal = harness.set_goal(agent, &["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    move_to(&mut harness, agent, "billing", "/repo/billing");
    crash_then_fail_the_predicate(&mut harness, agent, goal, turn);

    assert_eq!(
        harness.supervisor.recovery_opportunity(agent),
        Err(RecoveryRefusal::StaleWork)
    );
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::RecoveryRequired(_)))
    );
    no_work_without_consent(&harness);
    assert_eq!(issued(&harness), 1);
}
