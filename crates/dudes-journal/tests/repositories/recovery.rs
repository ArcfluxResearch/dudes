use super::*;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::*;
use dudes_proto::recovery_state::RecoveryClaimState;
use dudes_proto::recovery_state::admission::RecoveryAuthority;
use dudes_proto::repository::{
    HireRepositories, RepositoryEntry, RepositoryTurnGrant, RepositoryTurnIssued,
};
use dudes_types::{Mode, Provenance, TurnId};

pub(super) fn authority() -> (Vec<Entry>, dudes_proto::repository::RepositoryHire) {
    let (mut prefix, mut hire) = plural_authority();
    let mut third = hire.repositories.repositories.entries()[1].clone();
    third.repo_id = dudes_proto::repository::RepoId::parse("third").unwrap();
    let copy = &mut third.value;
    copy.origin.directory = copy.origin.directory.with_file_name("third");
    copy.origin.common_dir = copy.origin.directory.join(".git");
    copy.origin.directory_identity.inode += 100;
    copy.origin.common_identity.inode += 100;
    copy.execution.directory = copy.execution.directory.with_file_name("third");
    copy.execution.git_dir = copy.origin.common_dir.join("worktrees/third");
    copy.execution.directory_identity.inode += 100;
    copy.execution.git_identity.inode += 100;
    copy.worktree = dudes_types::WorktreeId::from_bytes([5; 16]);
    let Record::ProjectRepositoriesRecorded { repositories, .. } = &mut prefix[0].record else {
        unreachable!()
    };
    let mut catalog = repositories.entries().to_vec();
    catalog.push(RepositoryEntry {
        repo_id: third.repo_id.clone(),
        value: dudes_proto::repository::CatalogRepository {
            label: "Third".into(),
            origin: copy.origin.clone(),
        },
    });
    *repositories = RepositorySet::new(catalog[1].repo_id.clone(), catalog).unwrap();
    let Record::RepositoryTrustSet { pin, .. } = &prefix[1].record else {
        unreachable!()
    };
    prefix.insert(
        3,
        entry(Record::RepositoryTrustSet {
            project: "service".into(),
            repo_id: third.repo_id.clone(),
            catalog_lsn: Lsn(1),
            origin: copy.origin.clone(),
            pin: pin.clone(),
        }),
    );
    prefix.push(entry(Record::WorktreeLeased {
        worktree: copy.worktree,
        origin: copy.origin.directory.clone(),
        path: copy.execution.directory.clone(),
        branch: "dudes/third".into(),
        base: copy.base,
        from: None,
    }));
    let mut copies = hire.repositories.repositories.entries().to_vec();
    copies.push(third);
    hire.repositories.repositories = RepositorySet::new(copies[1].repo_id.clone(), copies).unwrap();
    (prefix, hire)
}

fn append(writer: &mut dudes_journal::Writer, store: &mut Projections, record: Record) -> Lsn {
    writer
        .checked_recovery(store)
        .unwrap()
        .append(&entry(record))
        .unwrap()
}

struct Scoped {
    _dir: tempfile::TempDir,
    writer: dudes_journal::Writer,
    store: Projections,
    binding: RecoveryBinding,
}

impl Scoped {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (start, _) = super::declared_tickets::seed_recovery(&journal);
        let (mut writer, _) = open(
            &journal,
            JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        writer
            .append(&entry(Record::TicketStartedWithRepositories(Box::new(
                start.clone(),
            ))))
            .unwrap();
        let mut store =
            Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        replay(&writer.reader(), &mut store).unwrap();
        let agent = store.agent(start.agent).unwrap().unwrap();
        let HireRepositories::Scoped(hire) = &agent.repositories else {
            panic!("scoped hire")
        };
        let round = store
            .goal(start.goal.id)
            .unwrap()
            .unwrap()
            .repository_round
            .unwrap();
        let grant = RepositoryTurnGrant::select(agent.hired_lsn, hire, Some(&round)).unwrap();
        let mut payload = samples::record(RecordKind::TurnIssued)
            .issue_payload()
            .unwrap()
            .to_owned();
        let dudes_proto::issue::ManagedIssuePayload::Ordinary {
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
        *owner = start.agent;
        *goal = start.goal.id;
        *model_selection = Some(dudes_types::ModelSelection::named("recorded-model"));
        *task = "original scoped task".into();
        answers.clear();
        let work = RecoveryWork {
            agent: start.agent,
            goal: start.goal.id,
            turn: *turn,
        };
        let issue_lsn = append(
            &mut writer,
            &mut store,
            Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued { grant, payload })),
        );
        let binding = RecoveryBinding {
            work,
            hired_lsn: agent.hired_lsn,
            issue_lsn,
            provider: agent.provider,
            worktree: agent.repositories.primary_worktree(),
            ticket: Some(dudes_proto::diff_stamp::TicketBinding {
                card: start.card,
                round: start.round,
                claimant: start.agent,
                goal: start.goal.id,
            }),
            model_selection: Some(dudes_types::ModelSelection::named("recorded-model")),
        };
        Self {
            _dir: dir,
            writer,
            store,
            binding,
        }
    }

    fn append(&mut self, record: Record) -> Lsn {
        append(&mut self.writer, &mut self.store, record)
    }
}

#[test]
fn scoped_recovery_cold_and_certified_warm_facts_keep_the_complete_original_authority() {
    let mut fixture = Scoped::new();
    let facts = fixture
        .writer
        .checked_recovery(&mut fixture.store)
        .unwrap()
        .facts(&fixture.binding)
        .unwrap();
    let RecoveryAuthority::Scoped { hire, grant } = &facts.authority else {
        panic!("scoped authority")
    };
    assert_eq!(hire.repositories.entries().len(), 3);
    assert_eq!(grant.repositories.entries().len(), 2);
    assert_ne!(
        hire.repositories.primary_id(),
        grant.repositories.primary_id()
    );
    for lease in [
        None,
        Some(grant.repositories.primary().worktree),
        Some(dudes_types::WorktreeId::from_bytes([99; 16])),
    ] {
        let mut binding = fixture.binding.clone();
        binding.worktree = lease;
        assert!(
            fixture
                .writer
                .checked_recovery(&mut fixture.store)
                .unwrap()
                .facts(&binding)
                .is_err()
        );
    }
    let mut cold = Projections::in_memory().unwrap();
    replay(&fixture.writer.reader(), &mut cold).unwrap();
    assert_eq!(
        facts,
        fixture
            .writer
            .checked_recovery(&mut cold)
            .unwrap()
            .facts(&fixture.binding)
            .unwrap()
    );
    assert_eq!(fixture.store.export().unwrap(), cold.export().unwrap());
    fixture.append(Record::WorktreeRemoved {
        worktree: hire.repositories.entries()[2].value.recorded.worktree,
        how: dudes_proto::WorktreeRemoval::Operator,
    });
    assert!(
        fixture
            .writer
            .checked_recovery(&mut fixture.store)
            .unwrap()
            .facts(&fixture.binding)
            .is_err()
    );
}

#[test]
fn an_undeclared_hired_lease_invalidates_pending_consent_after_the_active_goal_changes() {
    let mut fixture = Scoped::new();
    let binding = fixture.binding.clone();
    let undeclared = fixture
        .store
        .agent(binding.work.agent)
        .unwrap()
        .unwrap()
        .repositories
        .worktrees()
        .last()
        .unwrap();
    fixture.append(Record::TurnSpawned {
        agent: binding.work.agent,
        turn: binding.work.turn,
        pid: 42,
        start_time: 10,
        boot: None,
        session: None,
        reissued: false,
        mcp_grant: None,
    });
    let end_lsn = fixture.append(ended(&binding));
    let pause = fixture.append(Record::Pause {
        agent: binding.work.agent,
        browser: None,
    });
    fixture.append(Record::RecoveryRequired(RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::Existing { lsn: pause },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Paused { end_lsn },
        predecessor: None,
    }));
    let resume = fixture.append(Record::Resume {
        agent: binding.work.agent,
        browser: None,
    });
    let episode = RecoveryEpisodeId {
        work: binding.work,
        origin: pause,
    };
    let request = Hex32::new([81; 32]);
    let authorized = fixture.append(Record::RecoveryAuthorized(Box::new(RecoveryAuthorized {
        episode,
        binding: binding.clone(),
        request_id: request,
        request_digest: Hex32::new([82; 32]),
        attribution: RecoveryAttribution::LocalOperator { pid: 42, uid: None },
        expected_control: Some(resume),
        resulting_control: Some(resume),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(81, 2),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    })));
    let removed = fixture.append(Record::WorktreeRemoved {
        worktree: undeclared,
        how: dudes_proto::WorktreeRemoval::Operator,
    });
    fixture.append(Record::GoalCleared {
        agent: binding.work.agent,
        goal: binding.work.goal,
    });
    fixture.append(Record::RecoveryInvalidated(RecoveryInvalidated {
        claim: RecoveryClaimRef {
            episode,
            authorization_lsn: authorized,
        },
        evidence: RecoveryInvalidationEvidence::RecordedChange {
            reason: RecoveryInvalidationReason::LeaseChanged,
            lsn: removed,
        },
    }));
    let receipt = fixture
        .writer
        .checked_recovery(&mut fixture.store)
        .unwrap()
        .receipt_by_request(request)
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.state,
        RecoveryClaimState::Invalidated { .. }
    ));
    let mut cold = Projections::in_memory().unwrap();
    replay(&fixture.writer.reader(), &mut cold).unwrap();
    assert_eq!(fixture.store.export().unwrap(), cold.export().unwrap());
}

fn ended(binding: &RecoveryBinding) -> Record {
    let mut ended = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome,
        session,
        session_usage,
        ..
    } = &mut ended
    else {
        unreachable!()
    };
    *agent = binding.work.agent;
    *turn = binding.work.turn;
    *mode = Mode::Managed;
    *provenance = Provenance::Observed;
    *outcome = Some(dudes_proto::TurnOutcome::NoResult);
    *session = None;
    *session_usage = None;
    ended
}
