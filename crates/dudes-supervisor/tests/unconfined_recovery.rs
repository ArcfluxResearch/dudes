#![allow(missing_docs)]
//! A multi-repository hire without confinement recovers an interrupted turn the way a
//! single-repository agent does, in the primary copy with every other copy added.

use std::path::{Path, PathBuf};

use dudes_proto::goal::Evidence;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::{RecoveryAttribution, RecoveryReason};
use dudes_proto::repository::RepositoryPolicy;
use dudes_proto::{
    BrowserNoteAttribution, ExitStatus, GoalBudget, Record, RecordKind, TurnOutcome,
};
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input, ProbeAnswer, RepositoryAdmission, SpendView};
use dudes_types::{AgentId, TurnId, Usd};

const LABELS: [&str; 3] = ["service", "web", "docs"];

fn root() -> PathBuf {
    std::env::temp_dir().join("unconfined-recovery")
}

fn copy_of(label: &str) -> PathBuf {
    root().join("copies").join(label)
}

fn goal_turn(harness: &mut Harness) -> (AgentId, TurnId) {
    let (agent, _) =
        harness.hire_with_repository_copies(RepositoryPolicy::OperatorTrustV1, &LABELS, &root());
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "make the tests pass".into(),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["false".into()],
            timeout_s: 600,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let turn = harness
        .open_turn(agent)
        .expect("the goal's turn was issued");
    (agent, turn)
}

fn restarted(
    crashed: &Harness,
    agent: AgentId,
    before_start: impl FnOnce(&mut Harness),
) -> Harness {
    let mut harness = Harness::from_prefix(crashed.journal.clone());
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(RepositoryAdmission::new([(agent, hired, true)]));
    before_start(&mut harness);
    harness.drive(Input::Started);
    harness
}

fn launched(harness: &Harness, turn: TurnId) -> Vec<&dudes_providers::launch::LaunchSpec> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, turn: t, .. } if *t == turn => Some(spec),
            _ => None,
        })
        .collect()
}

fn add_dirs(spec: &dudes_providers::launch::LaunchSpec) -> Vec<&Path> {
    spec.argv
        .windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| Path::new(&pair[1]))
        .collect()
}

fn recover(harness: &mut Harness, agent: AgentId, choice: RecoveryChoice) {
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .expect("the interrupted turn is recoverable")
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
}

#[test]
fn a_turn_issued_before_the_crash_is_reissued_once_in_the_primary_copy() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, turn) = goal_turn(&mut harness);
    let issued = harness.supervisor.world().turns[&turn].clone();

    let mut harness = restarted(&harness, agent, |_| {});
    assert!(
        launched(&harness, turn).is_empty(),
        "restart alone repeats nothing"
    );
    recover(&mut harness, agent, RecoveryChoice::RepeatIssuedFresh);

    let spawns = launched(&harness, turn);
    assert_eq!(spawns.len(), 1, "one re-issue under the same turn");
    assert_eq!(spawns[0].cwd, copy_of("service"));
    let dirs = add_dirs(spawns[0]);
    assert_eq!(&dirs[dirs.len() - 2..], [copy_of("web"), copy_of("docs")]);
    let again = &harness.supervisor.world().turns[&turn];
    assert_eq!(again.task, issued.task, "the same task");
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::TurnIssued { .. }))
    );
    assert!(harness.records().any(
        |record| matches!(record, Record::TurnSpawned { turn: t, reissued: true, .. } if *t == turn)
    ));
}

#[test]
fn a_child_gone_after_the_crash_is_vanished_and_recovery_is_required() {
    let mut harness = Harness::new();
    let (agent, turn) = goal_turn(&mut harness);
    let pid = harness.supervisor.world().turns[&turn]
        .pid
        .expect("spawned");
    let mut harness = restarted(&harness, agent, |restarted| {
        restarted.host.answer(pid, ProbeAnswer::NoSuchPid);
    });
    let goal = harness.supervisor.world().turns[&turn]
        .goal
        .expect("goal turn");
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["false".into()],
            program: None,
            exit: Some(1),
            output_tail: String::new(),
        },
    });

    assert!(harness.records().any(|record| matches!(record,
        Record::TurnEnded { turn: t, outcome: Some(TurnOutcome::Vanished), exit, .. }
            if *t == turn && *exit == ExitStatus::UNKNOWN)));
    assert!(harness.records().any(|record| matches!(record,
        Record::RecoveryRequired(required)
            if required.binding.work.agent == agent
                && required.binding.work.turn == turn
                && required.reason == RecoveryReason::Vanished)));
    assert!(
        launched(&harness, turn).is_empty(),
        "nothing launches before consent"
    );

    recover(&mut harness, agent, RecoveryChoice::FreshWithRecap);
    let later: Vec<_> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec),
            _ => None,
        })
        .collect();
    assert_eq!(later.len(), 1, "the consented repeat launches one child");
    assert_eq!(later[0].cwd, copy_of("service"));
    let dirs = add_dirs(later[0]);
    assert_eq!(&dirs[dirs.len() - 2..], [copy_of("web"), copy_of("docs")]);
}

#[test]
fn a_copy_missing_at_restart_holds_the_agent_instead_of_recovering() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, turn) = goal_turn(&mut harness);
    let harness = restarted(&harness, agent, |restarted| {
        let primary = restarted.supervisor.world().agents[&agent]
            .repositories
            .primary_worktree()
            .expect("a hired copy");
        restarted.drive(Input::MissingWorktrees(vec![(agent, primary)]));
    });
    assert!(harness.records().any(|record| matches!(record,
        Record::AgentHeld { agent: held, cause: dudes_proto::AutomaticHold::WorktreeMissing { .. }, .. }
            if *held == agent)));
    assert!(launched(&harness, turn).is_empty());
}

fn chat_turn_crashed_before_spawn(unconfined: bool) -> (Vec<RecordKind>, usize, Vec<RecordKind>) {
    let mut harness = Harness::new();
    harness.launch = false;
    let agent = if unconfined {
        harness
            .hire_with_repository_copies(RepositoryPolicy::OperatorTrustV1, &LABELS, &root())
            .0
    } else {
        harness.hire()
    };
    harness.drive(Input::Spend {
        agent,
        view: SpendView::default(),
    });
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: "Who are you?".to_owned(),
        browser: BrowserNoteAttribution {
            request_id_digest: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            session_digest: Hex32::new([3; 32]),
        },
        context_lsn,
        context_reset_turn: None,
    }));
    let before: Vec<_> = harness.records().map(Record::kind).collect();
    let mut restarted = if unconfined {
        restarted(&harness, agent, |_| {})
    } else {
        Harness::from_prefix(harness.journal.clone())
    };
    if !unconfined {
        restarted.drive(Input::Started);
    }
    let after: Vec<_> = restarted.records().map(Record::kind).collect();
    let spawns = restarted
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Spawn { .. }))
        .count();
    (before, spawns, after)
}

#[test]
fn a_chat_turn_crashed_before_spawn_settles_as_it_does_for_a_single_repository_agent() {
    let (single_before, single_spawns, single_after) = chat_turn_crashed_before_spawn(false);
    let (unconfined_before, unconfined_spawns, unconfined_after) =
        chat_turn_crashed_before_spawn(true);
    assert!(single_before.contains(&RecordKind::IdentifiedConversationTurnIssued));
    assert!(unconfined_before.contains(&RecordKind::TurnIssuedWithRepositories));
    assert_eq!(
        (unconfined_spawns, single_spawns),
        (1, 1),
        "restart re-issues the unspawned chat turn once"
    );
    let tail = |before: &[RecordKind], after: &[RecordKind]| {
        after.get(before.len()..).unwrap_or_default().to_vec()
    };
    assert_eq!(
        tail(&unconfined_before, &unconfined_after),
        tail(&single_before, &single_after)
    );
}
