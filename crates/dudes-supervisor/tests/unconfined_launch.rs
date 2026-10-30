#![allow(missing_docs)]
//! A multi-repository hire without confinement launches every turn like a single-repository
//! agent in its own copy: in the primary copy, with each other hired copy added.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dudes_proto::record::Hex32;
use dudes_proto::repository::RepositoryPolicy;
use dudes_proto::{BrowserNoteAttribution, Record};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input, SpendView};
use dudes_types::{AgentId, Lsn, WorktreeId};

const LABELS: [&str; 3] = ["service", "web", "docs"];

fn root() -> PathBuf {
    std::env::temp_dir().join("unconfined-launch")
}

fn copy_of(label: &str) -> PathBuf {
    root().join("copies").join(label)
}

fn hire_into(harness: &mut Harness, policy: RepositoryPolicy, labels: &[&str]) -> (AgentId, Lsn) {
    harness.hire_with_repository_copies(policy, labels, &root())
}

fn queue_chat(harness: &mut Harness, agent: AgentId, text: &str) {
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: text.to_owned(),
        browser: BrowserNoteAttribution {
            request_id_digest: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            session_digest: Hex32::new([3; 32]),
        },
        context_lsn,
        context_reset_turn: None,
    }));
}

fn spawns(harness: &Harness) -> Vec<&dudes_providers::launch::LaunchSpec> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec),
            _ => None,
        })
        .collect()
}

fn add_dirs(argv: &[OsString]) -> Vec<&Path> {
    argv.windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| Path::new(&pair[1]))
        .collect()
}

#[test]
fn a_chat_turn_starts_in_the_primary_copy_with_each_other_copy_added_in_order() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::OperatorTrustV1, &LABELS);
    harness.drive(Input::Spend {
        agent,
        view: SpendView::default(),
    });
    queue_chat(&mut harness, agent, "Who are you?");

    let launched = spawns(&harness);
    assert_eq!(launched.len(), 1, "one chat turn launched");
    let spec = launched[0];
    assert_eq!(spec.cwd, copy_of("service"));
    let dirs = add_dirs(&spec.argv);
    assert_eq!(dirs.len(), 3, "chat files, then the two other copies");
    assert!(dirs[0].ends_with("files"));
    assert_eq!(dirs[1], copy_of("web"));
    assert_eq!(dirs[2], copy_of("docs"));

    let task = spec.argv.last().unwrap().to_str().unwrap();
    let end = spec.argv.len() - 1;
    assert_eq!(spec.argv[end - 1], "--");
    assert!(
        spec.argv[..end - 1].iter().all(|part| part != task),
        "the task is passed once, after the separator, and nothing follows it"
    );
    let section = format!(
        "service: {}\nweb: {}\ndocs: {}",
        copy_of("service").display(),
        copy_of("web").display(),
        copy_of("docs").display()
    );
    assert!(
        task.contains(&format!("Your repositories follow, primary first, one `label: path` line each.]\n\n{section}\n\nWho are you?")),
        "{task}"
    );
}

#[test]
fn a_general_goal_turn_starts_in_the_primary_copy() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::OperatorTrustV1, &LABELS);
    harness.set_goal(agent, &["true"]);
    let launched = spawns(&harness);
    assert_eq!(launched.len(), 1, "the goal's first turn launched");
    assert_eq!(launched[0].cwd, copy_of("service"));
    assert_eq!(
        add_dirs(&launched[0].argv)[1..],
        [copy_of("web").as_path(), copy_of("docs").as_path()]
    );
    let task = launched[0].argv.last().unwrap().to_str().unwrap();
    assert!(task.starts_with("[You are Kim"));
    assert!(task.contains("\n\nservice: "));
    assert!(task.contains("`dudes memory read`"), "the legacy teaching");
}

#[test]
fn a_confined_hire_never_takes_the_ordinary_launch() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::WorkingTreeV1, &LABELS);
    harness.set_goal(agent, &["true"]);
    assert!(spawns(&harness).is_empty());
    assert!(
        harness
            .trace
            .iter()
            .all(|seen| !matches!(seen, Seen::Spawn { .. })),
        "a confined hire reaches the scoped path or nothing"
    );
}

#[test]
fn a_goal_that_fills_the_cap_still_carries_the_section_and_stays_within_the_task_limit() {
    let mut harness = Harness::new();
    let (agent, _) = hire_into(&mut harness, RepositoryPolicy::OperatorTrustV1, &LABELS);
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "g".repeat(80_000),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 60,
        },
        budget: dudes_proto::GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let task = harness
        .records()
        .find_map(|record| match record {
            Record::LoopDecided {
                decision: dudes_proto::LoopDecision::Continue { task },
                ..
            } => Some(task.clone()),
            _ => None,
        })
        .expect("the goal's first decision");
    assert!(task.len() <= 65_536, "{}", task.len());
    assert!(task.contains("\n\nservice: "));
    assert!(
        task.ends_with("or its boundaries.]"),
        "the teaching is last"
    );
}

#[test]
fn a_missing_copy_of_an_unconfined_hire_holds_the_agent_before_any_launch() {
    let mut recorded = Harness::new();
    let (agent, _) = hire_into(&mut recorded, RepositoryPolicy::OperatorTrustV1, &LABELS);
    let mut restarted = Harness::from_prefix(recorded.journal.clone());
    restarted.drive(Input::MissingWorktrees(vec![(
        agent,
        WorktreeId::from_bytes([3; 16]),
    )]));
    restarted.drive(Input::Started);
    queue_chat(&mut restarted, agent, "Are you there?");
    assert!(restarted.records().any(|record| matches!(record,
        Record::AgentHeld { agent: held, cause: dudes_proto::AutomaticHold::WorktreeMissing { .. }, .. }
            if *held == agent)));
    assert!(spawns(&restarted).is_empty());
}
