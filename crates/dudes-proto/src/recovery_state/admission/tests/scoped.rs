use super::*;
use crate::repository::{
    HiredRepository, OwnedRepository, RepoId, RepositoryEntry, RepositorySet,
    ScopedHireRepositories, TicketRepositories, TicketRepositoryRound,
};

struct ScopedFixture {
    legacy: Fixture,
    copies: Vec<RepositoryEntry<OwnedRepository>>,
    repositories: TurnRepositories,
    ticket: TicketBinding,
}

impl ScopedFixture {
    fn new() -> Self {
        let mut legacy = Fixture::new();
        let Record::AgentHiredWithRepositories(mut hire) =
            samples::record(RecordKind::AgentHiredWithRepositories)
        else {
            unreachable!()
        };
        hire.id = legacy.binding.work.agent;
        hire.provider = legacy.binding.provider;
        hire.repositories.catalog_lsn = Lsn(1);
        let copies: Vec<_> = (0..3u8)
            .map(|index| {
                let mut copy = hire.repositories.repositories.entries()[0].clone();
                copy.repo_id = RepoId::parse(&format!("copy{index}")).unwrap();
                copy.value.worktree = WorktreeId::from_bytes([index + 1; 16]);
                copy.value.execution.directory.push(format!("copy{index}"));
                copy.value.execution.git_dir.push(format!("copy{index}"));
                copy.value.origin.directory.push(format!("copy{index}"));
                copy.value.origin.common_dir.push(format!("copy{index}"));
                copy.value.trust_lsn = Lsn(5 + u64::from(index));
                copy
            })
            .collect();
        hire.repositories.repositories =
            RepositorySet::new(copies[0].repo_id.clone(), copies.clone()).unwrap();
        let Record::WorkspaceTrustSet { pin: Some(pin), .. } =
            samples::record(RecordKind::WorkspaceTrustSet)
        else {
            unreachable!()
        };
        let normalized = ScopedHireRepositories {
            project: hire.repositories.project.clone(),
            catalog_lsn: hire.repositories.catalog_lsn,
            policy: hire.repositories.policy,
            repositories: RepositorySet::new(
                copies[0].repo_id.clone(),
                copies
                    .iter()
                    .map(|copy| RepositoryEntry {
                        repo_id: copy.repo_id.clone(),
                        value: HiredRepository::new(copy.value.clone(), pin.clone()),
                    })
                    .collect(),
            )
            .unwrap(),
        };
        let Record::TicketStartedWithRepositories(mut start) =
            samples::record(RecordKind::TicketStartedWithRepositories)
        else {
            unreachable!()
        };
        start.agent = legacy.binding.work.agent;
        start.goal = legacy.goal.clone();
        start.repositories.hired_lsn = legacy.binding.hired_lsn;
        start
            .repositories
            .declaration
            .project
            .clone_from(&normalized.project.id);
        start.repositories.declaration.catalog_lsn = normalized.catalog_lsn;
        start.repositories.declaration.repositories = RepositorySet::new(
            copies[1].repo_id.clone(),
            copies[1..]
                .iter()
                .map(|copy| RepositoryEntry {
                    repo_id: copy.repo_id.clone(),
                    value: copy.value.origin.clone(),
                })
                .collect(),
        )
        .unwrap();
        start.repositories.check_repo = copies[1].repo_id.clone();
        start.repositories.check_root = copies[1].value.execution.clone();
        let ticket = TicketBinding {
            card: start.card,
            goal: start.goal.id,
            claimant: start.agent,
            round: start.round,
        };
        let round = TicketRepositoryRound {
            card: start.card,
            goal: start.goal.id,
            start_lsn: Lsn(20),
            round: start.round,
            repositories: Some(TicketRepositories::Declared(
                crate::repository::TicketRepositoryScope {
                    declaration: start.repositories.declaration.clone(),
                    check_repo: start.repositories.check_repo.clone(),
                },
            )),
            authority: Some(start.repositories.clone()),
        };
        let grant = RepositoryTurnGrant::select(Lsn(10), &normalized, Some(&round)).unwrap();
        let repositories = TurnRepositories::scoped(&HireRepositories::Scoped(normalized), &grant);
        let payload = Record::TurnIssued {
            agent: legacy.binding.work.agent,
            turn: legacy.binding.work.turn,
            goal: legacy.binding.work.goal,
            mode: Mode::Managed,
            lsn_of_decision: Lsn(25),
            task: "exact scoped task".into(),
            session: Some(SessionId::new("original-session")),
            model_selection: legacy.binding.model_selection.clone(),
            answers: vec![Lsn(15), Lsn(16)],
        }
        .issue_payload()
        .unwrap()
        .to_owned();
        let issue =
            Record::TurnIssuedWithRepositories(Box::new(crate::repository::RepositoryTurnIssued {
                grant,
                payload,
            }));
        legacy.history = RecoveryHistory::default();
        for (index, copy) in copies.iter().enumerate() {
            legacy
                .history
                .observe(
                    Lsn(index as u64 + 2),
                    &Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record: Record::WorktreeLeased {
                            worktree: copy.value.worktree,
                            origin: copy.value.origin.directory.clone(),
                            path: copy.value.execution.directory.clone(),
                            branch: format!("copy{index}"),
                            base: copy.value.base,
                            from: None,
                        },
                    },
                    hash,
                )
                .unwrap();
        }
        for (at, record) in [
            (10, Record::AgentHiredWithRepositories(hire)),
            (20, Record::TicketStartedWithRepositories(start)),
            (30, issue),
        ] {
            legacy
                .history
                .observe(
                    Lsn(at),
                    &Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record,
                    },
                    hash,
                )
                .unwrap();
        }
        for at in [15, 16] {
            let mut record = samples::record(RecordKind::HumanAnswer);
            let Record::HumanAnswer { agent, .. } = &mut record else {
                unreachable!()
            };
            *agent = legacy.binding.work.agent;
            legacy
                .history
                .observe(
                    Lsn(at),
                    &Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record,
                    },
                    hash,
                )
                .unwrap();
        }
        legacy.binding.ticket = Some(ticket);
        legacy.binding.worktree = Some(copies[0].value.worktree);
        legacy.issue = legacy.history.at(Lsn(30)).unwrap().issue().unwrap();
        Self {
            legacy,
            copies,
            repositories,
            ticket,
        }
    }

    fn current(&self) -> RecoveryCurrent<'_> {
        let mut current = self.legacy.current();
        current.agent.worktree = self.legacy.binding.worktree;
        current.ticket = Some(self.ticket);
        current.repositories = &self.repositories;
        current.leases = self
            .copies
            .iter()
            .enumerate()
            .map(|(index, copy)| RepositoryLease {
                worktree: copy.value.worktree,
                leased_lsn: Lsn(index as u64 + 2),
                origin: &copy.value.origin.directory,
                path: &copy.value.execution.directory,
                base: copy.value.base,
                agent: Some(self.ticket.claimant),
                removed: false,
            })
            .collect();
        current
    }
}

#[test]
fn scoped_authority_keeps_all_hired_copies_and_the_independent_frozen_primary() {
    let fixture = ScopedFixture::new();
    let facts = fixture
        .current()
        .bind(&fixture.legacy.binding, &fixture.legacy.history)
        .unwrap();
    let RecoveryAuthority::Scoped { hire, grant } = facts.authority else {
        panic!("scoped authority")
    };
    assert_eq!(hire.repositories.entries().len(), 3);
    assert_eq!(grant.repositories.entries().len(), 2);
    assert_ne!(
        hire.repositories.primary_id(),
        grant.repositories.primary_id()
    );
    assert_eq!(facts.source_issue, fixture.legacy.issue);
    assert_eq!(facts.source_issue.issued.answers, [Lsn(15), Lsn(16)]);
    assert_eq!(
        fixture
            .legacy
            .history
            .hired_worktrees(&fixture.legacy.binding)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn recovery_refuses_an_undeclared_lease_change_and_substituted_authority() {
    let fixture = ScopedFixture::new();
    for change in 0..6 {
        let mut current = fixture.current();
        match change {
            0 => {
                current.leases.remove(0);
            }
            1 => current.leases[0].removed = true,
            2 => current.leases[0].path = current.leases[1].path,
            3 => current.leases[0].origin = current.leases[1].origin,
            4 => current.leases[0].agent = None,
            5 => current.repositories = &fixture.legacy.repositories,
            _ => unreachable!(),
        }
        assert_eq!(
            current
                .bind(&fixture.legacy.binding, &fixture.legacy.history)
                .unwrap_err(),
            RecoveryStateError::Binding,
            "change {change}"
        );
    }
    for lease in [None, Some(fixture.copies[1].value.worktree)] {
        let mut binding = fixture.legacy.binding.clone();
        binding.worktree = lease;
        assert!(
            fixture
                .current()
                .bind(&binding, &fixture.legacy.history)
                .is_err()
        );
    }
    let mut history = fixture.legacy.history.clone();
    history.witnesses.remove(&Lsn(20));
    assert!(
        fixture
            .current()
            .bind(&fixture.legacy.binding, &history)
            .is_err()
    );
}
