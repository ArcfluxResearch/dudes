use super::*;
use dudes_proto::repository::RepositoryPolicy;

fn unconfined(hire: &dudes_proto::repository::RepositoryHire) -> Entry {
    let mut hire = hire.clone();
    hire.repositories.policy = RepositoryPolicy::OperatorTrustV1;
    entry(Record::AgentHiredWithUnconfinedRepositories(Box::new(hire)))
}

fn replayed(
    hired: impl Fn(dudes_proto::repository::RepositoryHire) -> Entry,
) -> (Projections, dudes_types::AgentId) {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let (mut prefix, mut hire) = authority();
    hire.repositories.catalog_lsn = writer.append(&prefix[0]).unwrap();
    let Record::RepositoryTrustSet { catalog_lsn, .. } = &mut prefix[1].record else {
        unreachable!()
    };
    *catalog_lsn = hire.repositories.catalog_lsn;
    let mut copy = hire.repositories.repositories.entries()[0].clone();
    copy.value.trust_lsn = writer.append(&prefix[1]).unwrap();
    hire.repositories.repositories = RepositorySet::new(copy.repo_id.clone(), vec![copy]).unwrap();
    writer.append(&prefix[2]).unwrap();
    let agent = hire.id;
    writer.append(&hired(hire)).unwrap();
    drop(writer);
    let reader = Reader::open(dir.path()).unwrap().0;
    let mut store = Projections::in_memory().unwrap();
    assert_eq!(replay(&reader, &mut store).unwrap().applied, 4);
    (store, agent)
}

#[test]
fn an_unconfined_hire_survives_the_journal_and_folds_like_a_confined_one() {
    let (store, agent) = replayed(|hire| unconfined(&hire));
    let (confined, _) = replayed(|hire| entry(Record::AgentHiredWithRepositories(Box::new(hire))));
    assert_eq!(RecordKind::AgentHiredWithUnconfinedRepositories.tag(), 113);
    let mut row = store.agent(agent).unwrap().unwrap();
    let expected = confined.agent(agent).unwrap().unwrap();
    assert_ne!(row, expected);
    let dudes_proto::repository::HireRepositories::Scoped(scope) = &mut row.repositories else {
        panic!("a project hire is scoped");
    };
    assert_eq!(scope.policy, RepositoryPolicy::OperatorTrustV1);
    scope.policy = RepositoryPolicy::WorkingTreeV1;
    assert_eq!(row, expected);
    assert_eq!(store.worktrees().unwrap(), confined.worktrees().unwrap());
}

#[test]
fn an_unconfined_hire_with_options_replays_and_a_confined_one_is_refused() {
    let with_options = |mut hire: dudes_proto::repository::RepositoryHire| {
        hire.isolation = Some(dudes_types::IsolationMode::Bare);
        hire.mcp_servers = vec![dudes_types::McpServerName::parse("memory").unwrap()];
        hire
    };
    let (store, agent) = replayed(|hire| unconfined(&with_options(hire)));
    assert!(store.agent(agent).unwrap().is_some());

    let (prefix, hire) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &prefix);
    let before = store.export().unwrap();
    let confined = entry(Record::AgentHiredWithRepositories(Box::new(with_options(
        hire,
    ))));
    assert!(matches!(
        store.apply(Lsn(4), 2, &confined),
        Err(Error::Repository {
            reason: RepositoryStateError::Options,
            ..
        })
    ));
    assert_eq!(store.export().unwrap(), before);
}

#[test]
fn a_hire_record_kind_must_match_its_recorded_policy() {
    let (prefix, hire) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &prefix);
    let before = store.export().unwrap();
    let mut confined_kind = hire.clone();
    confined_kind.repositories.policy = RepositoryPolicy::OperatorTrustV1;
    for wrong in [
        entry(Record::AgentHiredWithRepositories(Box::new(confined_kind))),
        entry(Record::AgentHiredWithUnconfinedRepositories(Box::new(hire))),
    ] {
        assert!(matches!(
            store.apply(Lsn(4), 2, &wrong),
            Err(Error::Repository {
                reason: RepositoryStateError::Policy,
                ..
            })
        ));
        assert_eq!(store.export().unwrap(), before);
    }
}

fn mismatched() -> [Entry; 2] {
    let (_, hire) = authority();
    let mut unconfined_policy = hire.clone();
    unconfined_policy.repositories.policy = RepositoryPolicy::OperatorTrustV1;
    [
        entry(Record::AgentHiredWithRepositories(Box::new(
            unconfined_policy,
        ))),
        entry(Record::AgentHiredWithUnconfinedRepositories(Box::new(hire))),
    ]
}

#[test]
fn the_writer_refuses_a_hire_whose_kind_and_policy_disagree() {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    for wrong in mismatched() {
        assert!(matches!(
            writer.append(&wrong),
            Err(Error::Repository {
                reason: RepositoryStateError::Policy,
                ..
            })
        ));
    }
}

fn write_frame(dir: &std::path::Path, header_kind: u16, entry: &Entry) {
    let mut bytes = Vec::new();
    dudes_journal::frame::encode(header_kind, &serde_json::to_vec(entry).unwrap(), &mut bytes)
        .unwrap();
    std::fs::write(dudes_journal::segment::segment_path(dir, 1), bytes).unwrap();
}

#[test]
fn the_reader_refuses_a_hire_whose_kind_and_policy_disagree_and_a_wrong_header_kind() {
    for wrong in mismatched() {
        let dir = tempfile::tempdir().unwrap();
        write_frame(dir.path(), wrong.record.kind().tag(), &wrong);
        assert!(matches!(
            Reader::open(dir.path()),
            Err(Error::Repository {
                reason: RepositoryStateError::Policy,
                ..
            })
        ));
    }
    let (_, hire) = authority();
    let mut unconfined = hire;
    unconfined.repositories.policy = RepositoryPolicy::OperatorTrustV1;
    let right = entry(Record::AgentHiredWithUnconfinedRepositories(Box::new(
        unconfined,
    )));
    let dir = tempfile::tempdir().unwrap();
    write_frame(dir.path(), RecordKind::Pause.tag(), &right);
    assert!(matches!(
        Reader::open(dir.path()),
        Err(Error::RepositoryFrame { .. })
    ));
}

mod interrupted_turn {
    use super::*;
    use dudes_proto::issue::ManagedIssuePayload;
    use dudes_proto::recovery::{RecoveryBinding, RecoveryWork};
    use dudes_proto::recovery_state::admission::RecoveryAuthority;
    use dudes_proto::repository::{RepositoryTurnGrant, RepositoryTurnIssued};

    #[expect(
        clippy::too_many_lines,
        reason = "one fixture records catalog, trust, hire, goal and interrupted issue"
    )]
    fn journal_with_interrupted_turn(
        dir: &std::path::Path,
    ) -> (Projections, dudes_journal::Writer, RecoveryBinding) {
        let (mut writer, _) = open(
            &dir.join("journal"),
            JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        let mut store = Projections::open_with(&dir.join("state.redb"), Fsync::None).unwrap();
        let (prefix, mut hire) = super::super::recovery::authority();
        let catalog_lsn = writer.append(&prefix[0]).unwrap();
        let mut trust = std::collections::BTreeMap::new();
        for entry in prefix.into_iter().skip(1) {
            let mut record = entry.record;
            if let Record::RepositoryTrustSet {
                catalog_lsn: at, ..
            } = &mut record
            {
                *at = catalog_lsn;
            }
            let lsn = writer.append(&super::entry(record.clone())).unwrap();
            if let Record::RepositoryTrustSet { repo_id, .. } = record {
                trust.insert(repo_id, lsn);
            }
        }
        hire.repositories.catalog_lsn = catalog_lsn;
        let copies = hire
            .repositories
            .repositories
            .entries()
            .iter()
            .map(|copy| {
                let mut copy = copy.clone();
                copy.value.trust_lsn = trust[&copy.repo_id];
                copy
            })
            .collect();
        hire.repositories.repositories =
            RepositorySet::new(hire.repositories.repositories.primary_id().clone(), copies)
                .unwrap();
        let agent = hire.id;
        hire.repositories.policy = RepositoryPolicy::OperatorTrustV1;
        let hired_lsn = writer
            .append(&super::entry(Record::AgentHiredWithUnconfinedRepositories(
                Box::new(hire),
            )))
            .unwrap();
        let Record::GoalSet { mut goal, .. } = samples::record(RecordKind::GoalSet) else {
            unreachable!()
        };
        goal.text = "fix the service".into();
        let goal_id = goal.id;
        writer
            .append(&super::entry(Record::GoalSet {
                agent,
                goal,
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            }))
            .unwrap();
        replay(&writer.reader(), &mut store).unwrap();
        let row = store.agent(agent).unwrap().unwrap();
        let dudes_proto::repository::HireRepositories::Scoped(scope) = &row.repositories else {
            panic!("a project hire is scoped");
        };
        let grant = RepositoryTurnGrant::select(hired_lsn, scope, None).unwrap();
        let mut payload = samples::record(RecordKind::TurnIssued)
            .issue_payload()
            .unwrap()
            .to_owned();
        let ManagedIssuePayload::Ordinary {
            agent: owner,
            goal,
            turn,
            model_selection,
            task,
            answers,
            ..
        } = &mut payload
        else {
            unreachable!()
        };
        *owner = agent;
        *goal = goal_id;
        *model_selection = Some(dudes_types::ModelSelection::named("recorded-model"));
        *task = "original task".into();
        answers.clear();
        let work = RecoveryWork {
            agent,
            goal: goal_id,
            turn: *turn,
        };
        let issue_lsn = writer
            .checked_recovery(&mut store)
            .unwrap()
            .append(&super::entry(Record::TurnIssuedWithRepositories(Box::new(
                RepositoryTurnIssued { grant, payload },
            ))))
            .unwrap();
        let binding = RecoveryBinding {
            work,
            hired_lsn,
            issue_lsn,
            provider: row.provider,
            worktree: row.repositories.primary_worktree(),
            ticket: None,
            model_selection: Some(dudes_types::ModelSelection::named("recorded-model")),
        };
        (store, writer, binding)
    }

    #[test]
    fn an_interrupted_unconfined_turn_keeps_the_whole_hire_cold_and_warm() {
        let dir = tempfile::tempdir().unwrap();
        let (mut warm, mut writer, binding) = journal_with_interrupted_turn(dir.path());
        let facts = writer
            .checked_recovery(&mut warm)
            .unwrap()
            .facts(&binding)
            .unwrap();
        let RecoveryAuthority::Scoped { hire, grant } = &facts.authority else {
            panic!("scoped authority")
        };
        assert_eq!(hire.policy, RepositoryPolicy::OperatorTrustV1);
        assert_eq!(hire.repositories.entries().len(), 3);
        assert_eq!(grant.repositories.entries().len(), 3);
        assert_eq!(
            grant.repositories.primary_id(),
            hire.repositories.primary_id()
        );
        assert_eq!(
            binding.worktree,
            Some(hire.repositories.primary().recorded.worktree)
        );

        let mut cold = Projections::in_memory().unwrap();
        replay(&writer.reader(), &mut cold).unwrap();
        assert_eq!(
            facts,
            writer
                .checked_recovery(&mut cold)
                .unwrap()
                .facts(&binding)
                .unwrap()
        );
        assert_eq!(warm.export().unwrap(), cold.export().unwrap());

        for lease in [None, Some(dudes_types::WorktreeId::from_bytes([99; 16]))] {
            let mut other = binding.clone();
            other.worktree = lease;
            assert!(
                writer
                    .checked_recovery(&mut cold)
                    .unwrap()
                    .facts(&other)
                    .is_err()
            );
        }
    }
}
