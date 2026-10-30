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
