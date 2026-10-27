use super::*;
use dudes_proto::repository::{
    RepoId, RepositoryEntry, RepositoryTicketCreated, RepositoryTicketEdited,
    RepositoryTicketStarted, TicketDeclaration, TicketRepositories, TicketStartRepositories,
};
use dudes_proto::{Actor, CheckBaseline, CompletionCheckRef};
use dudes_types::{CardId, CardStatus, GoalId, Seq};

fn options() -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 0,
        ..JournalOptions::default()
    }
}

fn append(writer: &mut dudes_journal::Writer, record: Record) -> Lsn {
    writer.append(&entry(record)).unwrap()
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixture records all preceding catalog, trust, hire and ticket joins"
)]
fn seed(journal: &std::path::Path) -> (RepositoryTicketStarted, RepositoryTicketCreated) {
    let (prefix, mut hire) = plural_authority();
    let (mut writer, _) = open(journal, options()).unwrap();
    let catalog_lsn = append(&mut writer, prefix[0].record.clone());
    let mut trust = std::collections::BTreeMap::new();
    for entry in prefix.into_iter().skip(1) {
        let mut record = entry.record;
        if let Record::RepositoryTrustSet {
            catalog_lsn: at, ..
        } = &mut record
        {
            *at = catalog_lsn;
        }
        let lsn = append(&mut writer, record.clone());
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
        RepositorySet::new(hire.repositories.repositories.primary_id().clone(), copies).unwrap();
    let hired_lsn = append(
        &mut writer,
        Record::AgentHiredWithRepositories(Box::new(hire.clone())),
    );
    let copies = hire.repositories.repositories.entries();
    let check_repo = copies[1].repo_id.clone();
    let check = dudes_proto::TicketCheck {
        id: "verify".into(),
        label: "verification passes".into(),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["/usr/bin/true".into()],
            timeout_s: 30,
        },
    };
    let created = RepositoryTicketCreated {
        card: CardId::from_parts(7, 1),
        title: "Repair both repositories".into(),
        description: String::new(),
        department: "eng".into(),
        declaration: TicketDeclaration {
            project: "service".into(),
            catalog_lsn,
            repositories: RepositorySet::new(
                copies[0].repo_id.clone(),
                copies
                    .iter()
                    .map(|copy| RepositoryEntry {
                        repo_id: copy.repo_id.clone(),
                        value: copy.value.origin.clone(),
                    })
                    .collect(),
            )
            .unwrap(),
        },
        done_when: check.clone(),
        check_repo: check_repo.clone(),
        by: Actor::System,
        request: None,
    };
    append(
        &mut writer,
        Record::TicketCreatedWithRepositories(Box::new(created.clone())),
    );
    append(
        &mut writer,
        Record::TicketEditedWithRepositories(Box::new(RepositoryTicketEdited {
            card: created.card,
            title: created.title.clone(),
            description: "Keep the recorded declaration.".into(),
            done_when: check.clone(),
            check_repo: check_repo.clone(),
            by: Actor::System,
            expected_seq: Seq::ZERO,
        })),
    );
    append(
        &mut writer,
        Record::TicketAssigned {
            card: created.card,
            agent: hire.id,
            by: Actor::System,
            message: None,
            expected_seq: Seq(1),
        },
    );
    let start = RepositoryTicketStarted {
        card: created.card,
        agent: hire.id,
        expected_seq: Seq(2),
        round: 1,
        goal: dudes_proto::Goal {
            id: GoalId::from_parts(8, 1),
            text: created.title.clone(),
            predicate: check.predicate.clone(),
            baseline: Some(CheckBaseline::MayAlreadyPass),
            completion_check: Some(CompletionCheckRef {
                id: check.id.clone(),
                label: check.label.clone(),
                revision: serde_json::from_value(serde_json::json!(
                    "3d945edc3336bca332addf3cb4879def7bcd841cdafbace8d54b2b6864ae7b5d"
                ))
                .unwrap(),
            }),
            budget: dudes_proto::GoalBudget {
                usd: None,
                tokens: None,
            },
            hop_cap: 4,
        },
        repositories: TicketStartRepositories {
            hired_lsn,
            declaration: created.declaration.clone(),
            check,
            check_repo,
            check_root: copies[1].value.execution.clone(),
        },
    };
    (start, created)
}

#[test]
fn declared_tickets_and_frozen_check_roots_survive_warm_and_empty_cache_replay() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let (start, created) = seed(&journal);
    let (mut writer, _) = open(&journal, options()).unwrap();
    let start_lsn = append(
        &mut writer,
        Record::TicketStartedWithRepositories(Box::new(start.clone())),
    );
    drop(writer);
    let reader = Reader::open(&journal).unwrap().0;
    let cache = dir.path().join("state.redb");
    let mut warm = Projections::open_with(&cache, Fsync::None).unwrap();
    replay(&reader, &mut warm).unwrap();
    let (card, row) = warm.ticket(start.card).unwrap().unwrap();
    assert_eq!(card.status, CardStatus::Doing);
    assert_eq!(card.seq, Seq(3));
    let scope = row.repositories.declared().unwrap();
    assert_eq!(scope.declaration, created.declaration);
    assert_eq!(scope.check_repo, start.repositories.check_repo);
    let goal = warm.goal(start.goal.id).unwrap().unwrap();
    let round = goal.repository_round.unwrap();
    assert_eq!(round.start_lsn, start_lsn);
    assert_eq!(round.round, 1);
    assert_eq!(round.authority, Some(start.repositories.clone()));
    assert_eq!(row.goal, Some(start.goal.id));
    assert_eq!(
        warm.agent(start.agent).unwrap().unwrap().goal,
        Some(start.goal.id)
    );
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["project"], "service");
    assert_eq!(
        serde_json::from_value::<dudes_proto::TicketRow>(json).unwrap(),
        row
    );
    let expected = warm.export().unwrap();
    drop(warm);
    let mut warm = Projections::open_with(&cache, Fsync::None).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut fresh = Projections::in_memory().unwrap();
    replay(&reader, &mut fresh).unwrap();
    assert_eq!(fresh.export().unwrap(), expected);
    assert!(matches!(row.repositories, TicketRepositories::Declared(_)));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one malformed-authority table shares warm-cursor and unchanged-journal checks"
)]
fn malformed_declared_authority_is_refused_even_behind_a_warm_cursor() {
    for case in 0..9 {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut start, mut created) = seed(&journal);
        let mut warm = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        replay(&Reader::open(&journal).unwrap().0, &mut warm).unwrap();
        let removed = if case == 7 {
            let owner = warm.agent(start.agent).unwrap().unwrap();
            let dudes_proto::repository::HireRepositories::Scoped(hire) = owner.repositories else {
                panic!("scoped hire")
            };
            Some(Record::WorktreeRemoved {
                worktree: hire
                    .repositories
                    .get(&start.repositories.check_repo)
                    .unwrap()
                    .worktree,
                how: dudes_proto::WorktreeRemoval::Operator,
            })
        } else {
            None
        };
        let bad = match case {
            0 => {
                let mut entries = created.declaration.repositories.entries().to_vec();
                entries[0].repo_id = RepoId::parse("outside").unwrap();
                created.card = CardId::from_parts(7, 2);
                created.declaration.repositories =
                    RepositorySet::new(entries[0].repo_id.clone(), entries).unwrap();
                Record::TicketCreatedWithRepositories(Box::new(created))
            }
            1 => {
                start.repositories.hired_lsn = Lsn(1);
                Record::TicketStartedWithRepositories(Box::new(start))
            }
            2 => {
                start.round = 2;
                Record::TicketStartedWithRepositories(Box::new(start))
            }
            3 => {
                start.repositories.check_root.directory_identity.inode += 500;
                Record::TicketStartedWithRepositories(Box::new(start))
            }
            4 => {
                start.repositories.declaration.catalog_lsn = Lsn(1);
                Record::TicketStartedWithRepositories(Box::new(start))
            }
            5 => Record::TicketEditedWithRepositories(Box::new(RepositoryTicketEdited {
                card: start.card,
                title: "Outside".into(),
                description: String::new(),
                done_when: created.done_when,
                check_repo: RepoId::parse("outside").unwrap(),
                by: Actor::System,
                expected_seq: Seq(2),
            })),
            6 => Record::TicketStarted {
                card: start.card,
                agent: start.agent,
                goal: start.goal,
                expected_seq: Seq(2),
            },
            7 => Record::TicketStartedWithRepositories(Box::new(start)),
            _ => {
                created.card = CardId::from_parts(7, 2);
                let mut entries = created.declaration.repositories.entries().to_vec();
                entries.reverse();
                created.declaration.repositories =
                    RepositorySet::new(entries[0].repo_id.clone(), entries).unwrap();
                Record::TicketCreatedWithRepositories(Box::new(created))
            }
        };
        let (mut writer, _) = open(&journal, options()).unwrap();
        if let Some(removed) = removed {
            append(&mut writer, removed);
        }
        let bad_lsn = append(&mut writer, bad);
        let tail = entry(Record::Pause {
            agent: dudes_types::AgentId::NIL,
            browser: None,
        });
        let (tail_lsn, crc) = writer.append_witnessed(&tail).unwrap();
        drop(writer);
        warm.apply(tail_lsn, crc, &tail).unwrap();
        assert!(warm.last_applied_lsn() > bad_lsn);
        let expected = warm.export().unwrap();
        let bytes = std::fs::read(dudes_journal::segment::segment_path(
            &journal,
            bad_lsn.segment(),
        ))
        .unwrap();
        assert!(
            matches!(Reader::open(&journal), Err(Error::Repository { .. })),
            "case {case}"
        );
        assert!(
            matches!(
                replay(&Reader::new(&journal), &mut warm),
                Err(Error::Repository { .. })
            ),
            "case {case}"
        );
        assert_eq!(warm.export().unwrap(), expected);
        assert_eq!(
            std::fs::read(dudes_journal::segment::segment_path(
                &journal,
                bad_lsn.segment()
            ))
            .unwrap(),
            bytes
        );
    }
}

#[test]
fn unresolved_declarations_hold_catalog_members_until_done_or_removed() {
    for resolution in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (start, _) = seed(&journal);
        let (mut writer, _) = open(&journal, options()).unwrap();
        append(
            &mut writer,
            Record::AgentRetired {
                id: start.agent,
                reason: "finished".into(),
                browser_retire: None,
            },
        );
        if resolution == 1 {
            append(
                &mut writer,
                Record::TicketRemoved {
                    card: start.card,
                    by: Actor::System,
                    expected_seq: Seq(3),
                },
            );
        }
        if resolution == 2 {
            append(
                &mut writer,
                Record::CardTransition {
                    card: start.card,
                    from: CardStatus::Todo,
                    to: CardStatus::Done,
                    by: Actor::System,
                    expected_seq: Seq(3),
                    reason: None,
                },
            );
        }
        drop(writer);
        let mut store = Projections::in_memory().unwrap();
        replay(&Reader::open(&journal).unwrap().0, &mut store).unwrap();
        let project = store.repository_projects().unwrap().pop().unwrap();
        let mut entries = project
            .revisions
            .last_key_value()
            .unwrap()
            .1
            .entries()
            .to_vec();
        entries.pop();
        let record = Record::ProjectRepositoriesRecorded {
            project: "service".into(),
            repositories: RepositorySet::new(entries[0].repo_id.clone(), entries).unwrap(),
        };
        let result = store.apply(Lsn(store.last_applied_lsn().0 + 1), 1, &entry(record));
        if resolution != 0 {
            result.unwrap();
        } else {
            assert!(matches!(
                result,
                Err(Error::Repository {
                    reason: RepositoryStateError::Referenced,
                    ..
                })
            ));
        }
    }
}

#[test]
fn each_declared_ticket_carrier_requires_its_exact_required_header() {
    for kind in [
        RecordKind::TicketCreatedWithRepositories,
        RecordKind::TicketEditedWithRepositories,
        RecordKind::TicketStartedWithRepositories,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let payload = serde_json::to_vec(&entry(samples::record(kind))).unwrap();
        let mut bytes = Vec::new();
        dudes_journal::frame::encode(kind.tag(), &payload, &mut bytes).unwrap();
        let path = dudes_journal::segment::segment_path(dir.path(), 1);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Reader::open(dir.path()), Err(Error::RepositoryFrame { .. })),
            "{kind:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        bytes.clear();
        let wrong_kind = if kind == RecordKind::TicketCreatedWithRepositories {
            RecordKind::TicketEditedWithRepositories
        } else {
            RecordKind::TicketCreatedWithRepositories
        };
        dudes_journal::frame::encode_required(wrong_kind, &payload, &mut bytes).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Reader::open(dir.path()), Err(Error::RepositoryFrame { .. })),
            "{kind:?}"
        );
    }
}
