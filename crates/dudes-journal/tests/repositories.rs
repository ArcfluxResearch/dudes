#![allow(missing_docs)]

use dudes_journal::{Error, Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::record::samples;
use dudes_proto::repository::RepositorySet;
use dudes_proto::repository_state::history::RepositoryStateError;
use dudes_proto::{Entry, Record, RecordKind};
use dudes_types::{Lsn, Timestamp};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    }
}

fn authority() -> (Vec<Entry>, dudes_proto::repository::RepositoryHire) {
    let Record::AgentHiredWithRepositories(mut hire) =
        samples::record(RecordKind::AgentHiredWithRepositories)
    else {
        unreachable!()
    };
    hire.repositories.catalog_lsn = Lsn(1);
    let mut copy = hire.repositories.repositories.entries()[0].clone();
    copy.value.trust_lsn = Lsn(2);
    hire.repositories.repositories =
        RepositorySet::new(copy.repo_id.clone(), vec![copy.clone()]).unwrap();
    let Record::RepositoryTrustSet {
        project,
        repo_id,
        origin,
        pin,
        ..
    } = samples::record(RecordKind::RepositoryTrustSet)
    else {
        unreachable!()
    };
    (
        vec![
            entry(samples::record(RecordKind::ProjectRepositoriesRecorded)),
            entry(Record::RepositoryTrustSet {
                project,
                repo_id,
                origin,
                pin,
                catalog_lsn: Lsn(1),
            }),
            entry(Record::WorktreeLeased {
                worktree: copy.value.worktree,
                origin: copy.value.origin.directory,
                path: copy.value.execution.directory,
                branch: "dudes/service".into(),
                base: copy.value.base,
                from: None,
            }),
        ],
        *hire,
    )
}

fn plural_authority() -> (Vec<Entry>, dudes_proto::repository::RepositoryHire) {
    let (mut prefix, mut hire) = authority();
    let lease = prefix.pop().unwrap();
    let mut copies = hire.repositories.repositories.entries().to_vec();
    let mut second = copies[0].clone();
    second.repo_id = dudes_proto::repository::RepoId::parse("secondary").unwrap();
    let repo = &mut second.value;
    repo.origin.directory = repo.origin.directory.with_file_name("secondary");
    repo.origin.common_dir = repo.origin.directory.join(".git");
    repo.origin.directory_identity.inode += 100;
    repo.origin.common_identity.inode += 100;
    repo.execution.directory = repo.execution.directory.with_file_name("secondary");
    repo.execution.git_dir = repo.origin.common_dir.join("worktrees/secondary");
    repo.execution.directory_identity.inode += 100;
    repo.execution.git_identity.inode += 100;
    repo.worktree = dudes_types::WorktreeId::from_bytes([4; 16]);
    repo.trust_lsn = Lsn(3);
    let first = samples::repository_catalog().entries()[0].clone();
    let catalog = RepositorySet::new(
        first.repo_id.clone(),
        vec![
            first,
            dudes_proto::repository::RepositoryEntry {
                repo_id: second.repo_id.clone(),
                value: dudes_proto::repository::CatalogRepository {
                    label: "Second".into(),
                    origin: repo.origin.clone(),
                },
            },
        ],
    )
    .unwrap();
    prefix[0] = entry(Record::ProjectRepositoriesRecorded {
        project: "service".into(),
        repositories: catalog,
    });
    let Record::RepositoryTrustSet { pin, .. } = &prefix[1].record else {
        unreachable!()
    };
    prefix.push(entry(Record::RepositoryTrustSet {
        project: "service".into(),
        repo_id: second.repo_id.clone(),
        catalog_lsn: Lsn(1),
        origin: repo.origin.clone(),
        pin: pin.clone(),
    }));
    prefix.push(lease);
    prefix.push(entry(Record::WorktreeLeased {
        worktree: repo.worktree,
        origin: repo.origin.directory.clone(),
        path: repo.execution.directory.clone(),
        branch: "dudes/secondary".into(),
        base: repo.base,
        from: None,
    }));
    copies.push(second);
    hire.repositories.repositories =
        RepositorySet::new(hire.repositories.repositories.primary_id().clone(), copies).unwrap();
    (prefix, hire)
}

fn second_grant(
    mut hire: dudes_proto::repository::RepositoryHire,
    lsn: Lsn,
) -> dudes_proto::repository::RepositoryHire {
    let mut copies = hire.repositories.repositories.entries().to_vec();
    copies[1].value.trust_lsn = lsn;
    hire.repositories.repositories =
        RepositorySet::new(hire.repositories.repositories.primary_id().clone(), copies).unwrap();
    hire
}

#[test]
fn all_repository_grants_before_the_first_lease_allow_the_atomic_hire() {
    let (prefix, hire) = plural_authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &prefix);
    store
        .apply(
            Lsn(6),
            2,
            &entry(Record::AgentHiredWithRepositories(Box::new(hire.clone()))),
        )
        .unwrap();
    let leases = store.worktrees().unwrap();
    assert_eq!(leases.len(), 2);
    assert!(leases.iter().all(|lease| lease.agent == Some(hire.id)));
}

#[test]
fn an_interleaved_second_grant_cannot_authorize_an_atomic_hire() {
    let (mut prefix, hire) = plural_authority();
    prefix.swap(2, 3);
    let hire = second_grant(hire, Lsn(4));
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &prefix);
    let before = store.export().unwrap();
    let result = store.apply(
        Lsn(6),
        2,
        &entry(Record::AgentHiredWithRepositories(Box::new(hire))),
    );
    assert!(
        matches!(
            result,
            Err(Error::Repository {
                reason: RepositoryStateError::Trust,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(store.export().unwrap(), before);
    assert_eq!(store.last_applied_lsn(), Lsn(5));
}

#[test]
fn a_grant_refreshed_after_the_first_lease_cannot_authorize_an_atomic_hire() {
    let (mut prefix, hire) = plural_authority();
    let regrant = prefix[2].clone();
    let mut revoke = regrant.clone();
    let Record::RepositoryTrustSet { pin, .. } = &mut revoke.record else {
        unreachable!()
    };
    *pin = None;
    prefix.insert(4, revoke);
    prefix.insert(5, regrant);
    let hire = second_grant(hire, Lsn(6));
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &prefix);
    let before = store.export().unwrap();
    let result = store.apply(
        Lsn(8),
        2,
        &entry(Record::AgentHiredWithRepositories(Box::new(hire))),
    );
    assert!(
        matches!(
            result,
            Err(Error::Repository {
                reason: RepositoryStateError::Trust,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(store.export().unwrap(), before);
    assert_eq!(store.last_applied_lsn(), Lsn(7));
}

fn apply_prefix(store: &mut Projections, entries: &[Entry]) {
    for (index, entry) in entries.iter().enumerate() {
        store.apply(Lsn(index as u64 + 1), 1, entry).unwrap();
    }
}

#[test]
fn the_atomic_hire_requires_every_exact_preceding_trust_and_lease() {
    let (entries, hire) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &entries);
    let before = store.export().unwrap();
    let mut wrong = hire.clone();
    let mut copy = wrong.repositories.repositories.entries()[0].clone();
    copy.value.trust_lsn = Lsn(1);
    wrong.repositories.repositories = RepositorySet::new(copy.repo_id.clone(), vec![copy]).unwrap();
    assert!(matches!(
        store.apply(
            Lsn(4),
            2,
            &entry(Record::AgentHiredWithRepositories(Box::new(wrong)))
        ),
        Err(Error::Repository {
            reason: RepositoryStateError::Trust,
            ..
        })
    ));
    assert_eq!(store.export().unwrap(), before);
    assert_eq!(store.last_applied_lsn(), Lsn(3));
    store
        .apply(
            Lsn(4),
            2,
            &entry(Record::AgentHiredWithRepositories(Box::new(hire.clone()))),
        )
        .unwrap();
    assert_eq!(store.worktrees().unwrap()[0].agent, Some(hire.id));
    assert_eq!(
        store
            .agent(hire.id)
            .unwrap()
            .unwrap()
            .repositories
            .worktrees()
            .count(),
        1
    );
}

#[test]
fn removing_a_repository_never_allows_its_id_to_be_rebound() {
    let (entries, _) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &entries);
    let mut origin = samples::repository_catalog().entries()[0].clone();
    origin.repo_id = dudes_proto::repository::RepoId::parse("replacement").unwrap();
    let replacement = RepositorySet::new(origin.repo_id.clone(), vec![origin.clone()]).unwrap();
    store
        .apply(
            Lsn(4),
            2,
            &entry(Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: replacement,
            }),
        )
        .unwrap();
    let before = store.export().unwrap();
    let mut changed = samples::repository_catalog().entries()[0].clone();
    changed.value.origin.directory_identity.inode += 1_000_000;
    let rebound = RepositorySet::new(changed.repo_id.clone(), vec![changed]).unwrap();
    assert!(matches!(
        store.apply(
            Lsn(5),
            3,
            &entry(Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: rebound
            })
        ),
        Err(Error::Repository {
            reason: RepositoryStateError::Rebound,
            ..
        })
    ));
    assert_eq!(store.export().unwrap(), before);
    assert_eq!(store.repository_projects().unwrap()[0].bindings.len(), 2);
}

#[test]
fn required_repository_headers_are_exact_and_malformed_history_cannot_hide_behind_a_warm_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let options = JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 0,
        ..JournalOptions::default()
    };
    let (mut writer, _) = open(&journal, options).unwrap();
    let catalog = entry(samples::record(RecordKind::ProjectRepositoriesRecorded));
    let (catalog_lsn, _) = writer.append_witnessed(&catalog).unwrap();
    let Record::RepositoryTrustSet {
        project,
        repo_id,
        origin,
        pin,
        ..
    } = samples::record(RecordKind::RepositoryTrustSet)
    else {
        unreachable!()
    };
    let trust = entry(Record::RepositoryTrustSet {
        project: project.clone(),
        repo_id: repo_id.clone(),
        origin: origin.clone(),
        pin: pin.clone(),
        catalog_lsn,
    });
    writer.append(&trust).unwrap();
    drop(writer);
    let reader = Reader::open(&journal).unwrap().0;
    let mut warm = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 2);
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);

    let (mut writer, _) = open(&journal, options).unwrap();
    let mut wrong = origin;
    wrong.directory_identity.inode += 1_000_000;
    let bad = entry(Record::RepositoryTrustSet {
        project,
        repo_id,
        origin: wrong,
        pin,
        catalog_lsn,
    });
    let bad_lsn = writer.append(&bad).unwrap();
    let tail = entry(Record::Pause {
        agent: dudes_types::AgentId::NIL,
        browser: None,
    });
    let (tail_lsn, tail_crc) = writer.append_witnessed(&tail).unwrap();
    drop(writer);
    warm.apply(tail_lsn, tail_crc, &tail).unwrap();
    assert!(warm.last_applied_lsn() > bad_lsn);
    let before = warm.export().unwrap();
    let path = dudes_journal::segment::segment_path(&journal, bad_lsn.segment());
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        Reader::open(&journal),
        Err(Error::Repository {
            reason: RepositoryStateError::Trust,
            ..
        })
    ));
    assert!(matches!(
        replay(&Reader::new(&journal), &mut warm),
        Err(Error::Repository {
            reason: RepositoryStateError::Trust,
            ..
        })
    ));
    assert_eq!(warm.export().unwrap(), before);
    assert_eq!(std::fs::read(path).unwrap(), bytes);

    let invalid = tempfile::tempdir().unwrap();
    let payload = serde_json::to_vec(&catalog).unwrap();
    let mut bytes = Vec::new();
    dudes_journal::frame::encode(
        RecordKind::ProjectRepositoriesRecorded.tag(),
        &payload,
        &mut bytes,
    )
    .unwrap();
    std::fs::write(
        dudes_journal::segment::segment_path(invalid.path(), 1),
        &bytes,
    )
    .unwrap();
    assert!(matches!(
        Reader::open(invalid.path()),
        Err(Error::RepositoryFrame { .. })
    ));
    assert_eq!(
        std::fs::read(dudes_journal::segment::segment_path(invalid.path(), 1)).unwrap(),
        bytes
    );
}

#[test]
fn unknown_repository_hire_fields_refuse_before_open_or_warm_replay_changes_state() {
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
    let hire = entry(Record::AgentHiredWithRepositories(Box::new(hire)));
    let hire_lsn = writer.append(&hire).unwrap();
    drop(writer);

    let reader = Reader::open(dir.path()).unwrap().0;
    let mut warm = Projections::in_memory().unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 4);
    let path = dudes_journal::segment::segment_path(dir.path(), hire_lsn.segment());
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(usize::try_from(hire_lsn.offset()).unwrap());
    let mut extended = serde_json::to_value(&hire).unwrap();
    extended["browser_onboarding"] = serde_json::json!({"attempt": "future"});
    dudes_journal::frame::encode_required(
        RecordKind::AgentHiredWithRepositories,
        &serde_json::to_vec(&extended).unwrap(),
        &mut bytes,
    )
    .unwrap();
    let tail = entry(Record::Pause {
        agent,
        browser: None,
    });
    let tail_lsn =
        Lsn::from_parts(hire_lsn.segment(), u64::try_from(bytes.len()).unwrap()).unwrap();
    let payload = serde_json::to_vec(&tail).unwrap();
    dudes_journal::frame::encode(tail.record.kind().tag(), &payload, &mut bytes).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    warm.apply(
        tail_lsn,
        dudes_journal::frame::crc32c(1, tail.record.kind().tag(), &payload),
        &tail,
    )
    .unwrap();
    assert!(warm.last_applied_lsn() > hire_lsn);
    let before = warm.export().unwrap();

    for result in [
        Reader::open(dir.path()).map(|_| ()),
        open(dir.path(), JournalOptions::default()).map(|_| ()),
        replay(&reader, &mut warm).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(Error::Decode { lsn, ref source })
                if lsn == hire_lsn
                    && source.to_string().contains("unknown field `browser_onboarding`")),
            "{result:?}"
        );
    }
    assert_eq!(warm.export().unwrap(), before);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn a_legacy_issue_cannot_reinterpret_a_scoped_hire_as_singleton_authority() {
    let (entries, hire) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &entries);
    store
        .apply(
            Lsn(4),
            2,
            &entry(Record::AgentHiredWithRepositories(Box::new(hire.clone()))),
        )
        .unwrap();
    let before = store.export().unwrap();
    let mut issue = samples::record(RecordKind::TurnIssued);
    let Record::TurnIssued { agent, .. } = &mut issue else {
        unreachable!()
    };
    *agent = hire.id;
    assert!(matches!(
        store.apply(Lsn(5), 3, &entry(issue)),
        Err(Error::Repository {
            reason: RepositoryStateError::LegacyIssue,
            ..
        })
    ));
    assert_eq!(store.export().unwrap(), before);
}

#[test]
fn a_hire_keeps_its_revision_across_label_edits_but_cannot_omit_a_new_member() {
    let (entries, hire) = authority();
    let mut store = Projections::in_memory().unwrap();
    apply_prefix(&mut store, &entries);
    let mut primary = samples::repository_catalog().entries()[0].clone();
    primary.value.label = "New label".into();
    let renamed = RepositorySet::new(primary.repo_id.clone(), vec![primary.clone()]).unwrap();
    store
        .apply(
            Lsn(4),
            1,
            &entry(Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: renamed,
            }),
        )
        .unwrap();
    store
        .apply(
            Lsn(5),
            1,
            &entry(Record::AgentHiredWithRepositories(Box::new(hire.clone()))),
        )
        .unwrap();
    assert_eq!(
        store
            .agent(hire.id)
            .unwrap()
            .unwrap()
            .repositories
            .workspace()
            .unwrap()
            .label,
        "Service"
    );

    let mut expanded = Projections::in_memory().unwrap();
    apply_prefix(&mut expanded, &entries);
    let mut added = primary.clone();
    added.repo_id = dudes_proto::repository::RepoId::parse("client").unwrap();
    added.value.origin.directory = added.value.origin.directory.with_file_name("client");
    added.value.origin.common_dir = added.value.origin.directory.join(".git");
    added.value.origin.directory_identity.inode += 100;
    added.value.origin.common_identity.inode += 100;
    expanded
        .apply(
            Lsn(4),
            1,
            &entry(Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: RepositorySet::new(primary.repo_id.clone(), vec![primary, added])
                    .unwrap(),
            }),
        )
        .unwrap();
    let before = expanded.export().unwrap();
    assert!(matches!(
        expanded.apply(
            Lsn(5),
            1,
            &entry(Record::AgentHiredWithRepositories(Box::new(hire)))
        ),
        Err(Error::Repository {
            reason: RepositoryStateError::Catalog,
            ..
        })
    ));
    assert_eq!(expanded.export().unwrap(), before);
}

#[test]
fn a_cache_without_lifetime_markers_rebuilds_retired_legacy_hire_identity() {
    use dudes_proto::repository_state::history::RepositoryHireClass;
    assert_eq!(dudes_journal::PROJECTION_VERSION, 69);
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let cache = dir.path().join("state.redb");
    let (mut writer, _) = open(
        &journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let hire = samples::record(RecordKind::AgentHired);
    let agent = hire.hire().unwrap().agent;
    writer.append(&entry(hire)).unwrap();
    writer
        .append(&entry(Record::AgentRetired {
            id: agent,
            reason: "completed".into(),
            browser_retire: None,
        }))
        .unwrap();
    drop(writer);
    let reader = Reader::open(&journal).unwrap().0;
    let mut store = Projections::open_with(&cache, Fsync::None).unwrap();
    replay(&reader, &mut store).unwrap();
    let expected = store.export().unwrap();
    drop(store);
    let db = redb::Database::create(&cache).unwrap();
    let txn = db.begin_write().unwrap();
    txn.delete_table(redb::TableDefinition::<&[u8; 16], &[u8]>::new(
        "repository_hires",
    ))
    .unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("meta"))
        .unwrap()
        .insert("schema_version", &61)
        .unwrap();
    txn.commit().unwrap();
    drop(db);
    let mut rebuilt = Projections::open_with(&cache, Fsync::None).unwrap();
    assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
    replay(&reader, &mut rebuilt).unwrap();
    assert!(rebuilt.agent(agent).unwrap().is_none());
    assert_eq!(
        rebuilt.repository_hire_class(agent).unwrap(),
        Some(RepositoryHireClass::Legacy)
    );
    assert_eq!(rebuilt.export().unwrap(), expected);
}

#[path = "repositories/declared_tickets.rs"]
mod declared_tickets;
