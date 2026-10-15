//! Workspace metadata preserves enrolled identity without disclosing working directories.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use duded::floor_actions::Profile;
use duded::ports::{Answered, Ask};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::record::RecordedWorkspace;
use dudes_proto::{AgentBudget, GoalBudget, Predicate, Record};
use dudes_types::{IsolationMode, Usd};

mod common;
use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

fn profile(id: &str, label: &str, cwd: PathBuf) -> Profile {
    Profile {
        id: id.into(),
        label: label.into(),
        checks: vec![duded::floor_actions::EnrolledCheck::enrolled(
            "Checks pass",
            Predicate::Command {
                argv: vec!["/usr/bin/true".into()],
                timeout_s: 10,
            },
        )],
        written_cwd: cwd.clone(),
        cwd,
        department: "engineering".into(),
        isolation: IsolationMode::Default,
        agent_budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        goal_budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 3,
        mark: None,
    }
}

#[test]
fn local_hires_record_only_unique_exact_enrollment_matches() {
    let fixture = Fixture::new();
    let root = fixture.state.path().canonicalize().unwrap();
    let unique = root.join("unique-directory-that-does-not-exist");
    let ambiguous = root.join("ambiguous-directory-that-does-not-exist");
    let mut settings = fixture.settings();
    settings.enrollment.profiles = vec![
        profile("alpha", "Workbench", unique.clone()),
        profile("beta", "Workbench", ambiguous.clone()),
        profile("gamma", "Other bench", ambiguous.clone()),
    ];
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let mut ids = Vec::new();
    for cwd in [
        &unique,
        &ambiguous,
        &root.join("not-enrolled"),
        &root.join(".."),
        &PathBuf::from("relative"),
    ] {
        let mut params = common::hire_params(fixture.state.path());
        params.cwd = cwd.clone();
        let Answered::Hired(result) = running.handle.ask(Ask::Hire(Box::new(params))).unwrap()
        else {
            panic!("hire result");
        };
        ids.push(result.agent);
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let workspaces = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired { id, workspace, .. } => Some((id, workspace)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = ids
        .into_iter()
        .zip([
            Some(RecordedWorkspace {
                id: "alpha".into(),
                label: "Workbench".into(),
            }),
            None,
            None,
            None,
            None,
        ])
        .collect::<Vec<_>>();
    assert_eq!(workspaces, expected);
    assert!(
        !unique.exists() && !ambiguous.exists(),
        "matching inspected no filesystem directory"
    );
}
