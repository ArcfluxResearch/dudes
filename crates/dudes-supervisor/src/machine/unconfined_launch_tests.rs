use super::*;
use crate::testing::Harness;
use dudes_proto::repository::{
    HireRepositories, RepoId, RepositoryPolicy, RepositorySet, RepositoryTurnGrant,
    TicketRepositoryRound,
};

const LABELS: [&str; 3] = ["service", "web", "docs"];

fn copy(label: &str) -> PathBuf {
    std::env::temp_dir()
        .join("unconfined-launch-unit")
        .join("copies")
        .join(label)
}

fn launched(
    grant: RepositoryTurnGrant,
    harness: &Harness,
    agent: AgentId,
) -> (PathBuf, Vec<PathBuf>) {
    let row = harness.supervisor.world().agents[&agent].clone();
    let ManagedLaunch::Ready(prepared) = harness.supervisor.launch(
        agent,
        &row,
        Some(&IssueRepositories::Scoped(Box::new(grant))),
        "task",
        &TurnSession::First(dudes_types::SessionId::from(
            "5fd0cb4a-2d31-4c2c-9e6a-0f2b2a1f7c11",
        )),
        &ModelSelection::provider_default(),
    ) else {
        panic!("an unconfined hire launches through the ordinary path");
    };
    let ManagedSpawn::Composed { spec, .. } = prepared.spawn else {
        panic!("an unconfined hire never takes the scoped launch");
    };
    let dirs = spec
        .argv
        .windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .skip(1)
        .map(|pair| PathBuf::from(&pair[1]))
        .collect();
    (spec.cwd, dirs)
}

fn fixture(policy: RepositoryPolicy) -> (Harness, AgentId, RepositoryTurnGrant) {
    let mut harness = Harness::new();
    let (agent, hired) = harness.hire_with_repository_copies(
        policy,
        &LABELS,
        &std::env::temp_dir().join("unconfined-launch-unit"),
    );
    let HireRepositories::Scoped(hire) = &harness.supervisor.world().agents[&agent].repositories
    else {
        unreachable!()
    };
    let grant = RepositoryTurnGrant::select(hired, hire, None).unwrap();
    (harness, agent, grant)
}

#[test]
fn a_general_turn_runs_in_the_hires_primary_copy_and_adds_the_others_in_order() {
    let (harness, agent, grant) = fixture(RepositoryPolicy::OperatorTrustV1);
    assert_eq!(
        launched(grant, &harness, agent),
        (copy("service"), vec![copy("web"), copy("docs")])
    );
}

#[test]
fn a_ticket_turn_runs_in_the_primary_copy_its_grant_selected() {
    let (harness, agent, mut grant) = fixture(RepositoryPolicy::OperatorTrustV1);
    let entries: Vec<_> = grant.repositories.entries().to_vec();
    grant.repositories = RepositorySet::new(RepoId::parse("web").unwrap(), entries).unwrap();
    grant.ticket = Some(TicketRepositoryRound {
        card: dudes_types::CardId::from_parts(1, 2),
        goal: dudes_types::GoalId::from_parts(1, 3),
        start_lsn: Lsn::from_parts(1, 9).unwrap(),
        round: 1,
        repositories: None,
        authority: None,
    });
    assert_eq!(
        launched(grant, &harness, agent),
        (copy("web"), vec![copy("service"), copy("docs")])
    );
}

#[test]
fn a_confined_grant_still_takes_the_scoped_launch() {
    let (harness, agent, grant) = fixture(RepositoryPolicy::WorkingTreeV1);
    let row = harness.supervisor.world().agents[&agent].clone();
    let ManagedLaunch::Ready(prepared) = harness.supervisor.launch(
        agent,
        &row,
        Some(&IssueRepositories::Scoped(Box::new(grant))),
        "task",
        &TurnSession::Assigned,
        &ModelSelection::provider_default(),
    ) else {
        panic!("a confined hire prepares a scoped launch");
    };
    assert!(matches!(prepared.spawn, ManagedSpawn::Scoped { .. }));
}

#[test]
fn a_grant_that_leaves_a_hired_copy_out_is_refused() {
    let (harness, agent, mut grant) = fixture(RepositoryPolicy::OperatorTrustV1);
    let kept: Vec<_> = grant.repositories.entries()[..2].to_vec();
    grant.repositories = RepositorySet::new(kept[0].repo_id.clone(), kept).unwrap();
    let row = harness.supervisor.world().agents[&agent].clone();
    assert!(matches!(
        harness.supervisor.launch(
            agent,
            &row,
            Some(&IssueRepositories::Scoped(Box::new(grant))),
            "task",
            &TurnSession::Assigned,
            &ModelSelection::provider_default(),
        ),
        ManagedLaunch::Refused(LaunchError::RepositoryAuthorityUnavailable)
    ));
}
