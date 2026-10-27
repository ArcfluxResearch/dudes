#![allow(missing_docs)]

use dudes_proto::record::samples;
use dudes_proto::{Entry, Record, RecordKind};
use dudes_supervisor::World;
use dudes_types::{Lsn, Timestamp};

#[test]
fn legacy_trust_revocation_and_regrant_keep_their_historical_singleton_semantics() {
    let mut world = World::default();
    let mut trust = samples::record(RecordKind::WorkspaceTrustSet);
    let Record::WorkspaceTrustSet {
        workspace_id,
        repository,
        ..
    } = &mut trust
    else {
        unreachable!()
    };
    *workspace_id = "service".into();
    let first = repository.clone().unwrap();
    world
        .apply_checked(
            Lsn(1),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: trust.clone(),
            },
        )
        .unwrap();
    assert_eq!(world.workspace_trust["service"].repository, first);
    world
        .apply_checked(
            Lsn(2),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::WorkspaceTrustSet {
                    workspace_id: "service".into(),
                    trusted: false,
                    repository: None,
                    pin: None,
                },
            },
        )
        .unwrap();
    let Record::WorkspaceTrustSet { repository, .. } = &mut trust else {
        unreachable!()
    };
    repository.as_mut().unwrap().directory_identity.inode += 1;
    let changed = repository.clone().unwrap();
    world
        .apply_checked(
            Lsn(3),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: trust,
            },
        )
        .unwrap();
    assert_eq!(world.workspace_trust["service"].repository, changed);
    assert!(world.repository_projects.is_empty());
    assert!(world.repository_trust.is_empty());
}
