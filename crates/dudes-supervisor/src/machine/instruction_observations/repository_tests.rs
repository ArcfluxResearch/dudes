use super::*;
use crate::input::Input;
use crate::testing::{Harness, Seen};
use dudes_proto::record::{FileIdentity, Hex32, RecordedWorkspace, TrustPin, TrustedRepository};
use dudes_proto::repository::{
    CatalogRepository, ExecutionRepository, HireRepositories, OwnedRepository, RepoId,
    RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet, RepositoryTicketCreated,
    RepositoryTicketStarted, RepositoryTurnIssued, ScopedRepositories, TicketDeclaration,
    TicketStartRepositories,
};
use dudes_proto::repository_instruction::{
    INSTRUCTION_BATCH_VERSION, RepositoryInstructionBinding, RepositoryInstructionSnapshot,
};
use dudes_proto::{Actor, AgentBudget, CheckBaseline, Goal, GoalBudget, Predicate, TicketCheck};
use dudes_types::{CardId, CardStatus, GitCommit, Seq, Timestamp};

fn append(harness: &mut Harness, record: Record) -> Lsn {
    harness.append_replayed(Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "one replayed catalog, hire and ticket prefix supplies the lifecycle controls"
)]
fn fixture() -> (Harness, AgentId, GoalId, RepositoryTurnGrant) {
    let mut harness = Harness::new();
    let repo_id = RepoId::parse("primary").unwrap();
    let origin = TrustedRepository {
        directory: std::env::temp_dir().join("instruction-origin"),
        common_dir: std::env::temp_dir().join("instruction-origin/.git"),
        directory_identity: FileIdentity {
            device: 1,
            inode: 10,
        },
        common_identity: FileIdentity {
            device: 1,
            inode: 11,
        },
    };
    let catalog = append(
        &mut harness,
        Record::ProjectRepositoriesRecorded {
            project: "service".into(),
            repositories: RepositorySet::new(
                repo_id.clone(),
                vec![RepositoryEntry {
                    repo_id: repo_id.clone(),
                    value: CatalogRepository {
                        label: "Primary".into(),
                        origin: origin.clone(),
                    },
                }],
            )
            .unwrap(),
        },
    );
    let trust = append(
        &mut harness,
        Record::RepositoryTrustSet {
            project: "service".into(),
            repo_id: repo_id.clone(),
            origin: origin.clone(),
            catalog_lsn: catalog,
            pin: Some(TrustPin {
                claude_tree: None,
                mcp_json: None,
                attributes: Hex32::new([7; 32]),
                config: Hex32::new([8; 32]),
            }),
        },
    );
    let worktree = WorktreeId::from_bytes([3; 16]);
    let path = std::env::temp_dir().join("instruction-copy");
    let base = GitCommit::parse("0000000000000000000000000000000000000000").unwrap();
    append(
        &mut harness,
        Record::WorktreeLeased {
            worktree,
            origin: origin.directory.clone(),
            path: path.clone(),
            branch: "dudes/030303030303".into(),
            base,
            from: None,
        },
    );
    let execution = ExecutionRepository {
        directory: path,
        directory_identity: FileIdentity {
            device: 1,
            inode: 20,
        },
        git_dir: origin.common_dir.join("worktrees/copy"),
        git_identity: FileIdentity {
            device: 1,
            inode: 21,
        },
    };
    let agent = AgentId::from_parts(1, 1);
    let hired = append(
        &mut harness,
        Record::AgentHiredWithRepositories(Box::new(RepositoryHire {
            id: agent,
            name: "Kim".into(),
            character: None,
            accent: None,
            role: "engineer".into(),
            department: "eng".into(),
            provider: Provider::Claude,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            max_turns: None,
            max_budget: None,
            model: None,
            repositories: ScopedRepositories {
                project: RecordedWorkspace {
                    id: "service".into(),
                    label: "Service".into(),
                },
                catalog_lsn: catalog,
                policy: RepositoryPolicy::WorkingTreeV1,
                repositories: RepositorySet::new(
                    repo_id.clone(),
                    vec![RepositoryEntry {
                        repo_id: repo_id.clone(),
                        value: OwnedRepository {
                            origin: origin.clone(),
                            execution: execution.clone(),
                            worktree,
                            base,
                            trust_lsn: trust,
                        },
                    }],
                )
                .unwrap(),
            },
        })),
    );
    let declaration = TicketDeclaration {
        project: "service".into(),
        catalog_lsn: catalog,
        repositories: RepositorySet::new(
            repo_id.clone(),
            vec![RepositoryEntry {
                repo_id: repo_id.clone(),
                value: origin,
            }],
        )
        .unwrap(),
    };
    let card = CardId::from_parts(1, 2);
    let check = TicketCheck {
        id: "changed".into(),
        label: "Files changed".into(),
        predicate: Predicate::Command {
            argv: vec!["verify-instructions".into()],
            timeout_s: 1,
        },
    };
    append(
        &mut harness,
        Record::TicketCreatedWithRepositories(Box::new(RepositoryTicketCreated {
            card,
            title: "Observe instructions".into(),
            description: String::new(),
            department: "eng".into(),
            declaration: declaration.clone(),
            done_when: check.clone(),
            check_repo: repo_id.clone(),
            by: Actor::System,
            request: None,
        })),
    );
    append(
        &mut harness,
        Record::TicketAssigned {
            card,
            agent,
            by: Actor::System,
            expected_seq: Seq::ZERO,
            priority: None,
            message: None,
        },
    );
    let goal = GoalId::from_parts(1, 3);
    append(
        &mut harness,
        Record::TicketStartedWithRepositories(Box::new(RepositoryTicketStarted {
            card,
            agent,
            expected_seq: Seq(1),
            round: 1,
            goal: Goal {
                id: goal,
                text: "Observe instructions".into(),
                predicate: check.predicate.clone(),
                baseline: Some(CheckBaseline::MayAlreadyPass),
                completion_check: Some(dudes_proto::CompletionCheckRef {
                    id: check.id.clone(),
                    label: check.label.clone(),
                    revision: Hex32::new([0; 32]),
                }),
                budget: GoalBudget {
                    usd: None,
                    tokens: None,
                },
                hop_cap: 4,
            },
            repositories: TicketStartRepositories {
                hired_lsn: hired,
                declaration,
                check,
                check_repo: repo_id,
                check_root: execution,
            },
        })),
    );
    let world = harness.supervisor.world();
    let HireRepositories::Scoped(hire) = &world.agents[&agent].repositories else {
        unreachable!()
    };
    let grant =
        RepositoryTurnGrant::select(hired, hire, world.active_repository_round(agent).unwrap())
            .unwrap();
    (harness, agent, goal, grant)
}

fn pending(
    harness: &mut Harness,
    agent: AgentId,
    grant: &RepositoryTurnGrant,
) -> (InstructionRequest, RepositoryInstructionStart) {
    let turn = TurnId::from_parts(1, 4);
    let payload = Record::TurnIssued {
        agent,
        turn,
        goal: grant.ticket.as_ref().unwrap().goal,
        task: "Observe instructions".into(),
        session: None,
        mode: Mode::Managed,
        model_selection: None,
        lsn_of_decision: Lsn(1),
        answers: Vec::new(),
    }
    .issue_payload()
    .unwrap()
    .to_owned();
    harness.supervisor.observe_before_issue(
        agent,
        turn,
        ModelSelection::provider_default(),
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
                grant: grant.clone(),
                payload,
            })),
        },
        Then::Fold,
    );
    let (request, owner) = harness
        .supervisor
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::SampleRepositoryInstructions { request, owner, .. } => Some((*request, *owner)),
            _ => None,
        })
        .unwrap();
    let sample = RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding: RepositoryInstructionBinding {
            hired_lsn: grant.hired_lsn,
            ticket_start_lsn: grant.ticket.as_ref().map(|round| round.start_lsn),
        },
        ticket: harness
            .supervisor
            .world()
            .instructions
            .association_for_goal(grant.ticket.as_ref().unwrap().goal, Seq(2)),
        owner,
        members: vec![RepositoryInstructionSnapshot {
            repo_id: grant.repositories.primary_id().clone(),
            root: None,
            snapshot: InstructionSnapshot {
                sampled_at: Timestamp::UNIX_EPOCH,
                source: InstructionSnapshotSource::Fresh,
                execution_folder: None,
                participants: Vec::new(),
                inputs: Vec::new(),
                inventoried: Vec::new(),
                gaps: vec![gap(InstructionGapKind::PathUnresolved, agent)],
                usage: InstructionSampleUsage::default(),
                pid_namespace: None,
                boot: None,
            },
        }],
    };
    (request, sample)
}

#[test]
fn obsolete_prospective_ticket_sample_releases_its_pending_start() {
    let (mut harness, agent, _, grant) = fixture();
    let (request, sample) = pending(&mut harness, agent, &grant);
    let ticket = sample.ticket.unwrap();
    append(
        &mut harness,
        Record::CardTransition {
            card: ticket.card,
            from: CardStatus::Doing,
            to: CardStatus::Done,
            expected_seq: Seq(2),
            by: Actor::System,
            reason: None,
        },
    );
    let effects = harness.step(Input::RepositoryInstructionsSampled {
        request,
        sample: Box::new(sample),
        comparisons: None,
    });
    assert!(
        !harness
            .supervisor
            .instructions
            .awaiting
            .contains_key(&request)
    );
    assert!(
        !harness
            .supervisor
            .instructions
            .pending_agents
            .contains(&agent)
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Seen::Spawn { .. }))
    );
}

#[test]
fn a_wrong_batch_binding_cannot_consume_the_original_sample() {
    let (mut harness, agent, _, grant) = fixture();
    let (request, sample) = pending(&mut harness, agent, &grant);
    let mut wrong = sample.clone();
    wrong.binding.hired_lsn = Lsn(999);
    let effects = harness.step(Input::RepositoryInstructionsSampled {
        request,
        sample: Box::new(wrong),
        comparisons: None,
    });
    assert!(!effects.iter().any(|effect| matches!(
        effect.record(),
        Some(Record::InstructionObservationBatchStarted(_))
    )));
    assert!(
        harness
            .supervisor
            .instructions
            .awaiting
            .contains_key(&request)
    );
    let mut wrong_assignment = sample.clone();
    wrong_assignment.ticket.as_mut().unwrap().assignment_lsn = Lsn(999);
    let effects = harness.step(Input::RepositoryInstructionsSampled {
        request,
        sample: Box::new(wrong_assignment),
        comparisons: None,
    });
    assert!(!effects.iter().any(|effect| matches!(
        effect.record(),
        Some(Record::InstructionObservationBatchStarted(_))
    )));
    assert!(
        harness
            .supervisor
            .instructions
            .awaiting
            .contains_key(&request)
    );
    harness.step(Input::RepositoryInstructionsSampled {
        request,
        sample: Box::new(sample),
        comparisons: None,
    });
    assert!(
        !harness
            .supervisor
            .instructions
            .awaiting
            .contains_key(&request)
    );
    assert!(
        harness
            .records()
            .any(|record| matches!(record, Record::InstructionObservationBatchStarted(_)))
    );
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::TurnIssuedWithRepositories(_)))
    );
}

#[test]
fn an_unavailable_prospective_sample_cancels_only_its_pending_start() {
    let (mut harness, agent, _, grant) = fixture();
    let (request, _) = pending(&mut harness, agent, &grant);
    harness.drive(Input::RepositoryInstructionsUnavailable { request });
    assert!(
        !harness
            .supervisor
            .instructions
            .awaiting
            .contains_key(&request)
    );
    assert!(
        !harness
            .supervisor
            .instructions
            .pending_agents
            .contains(&agent)
    );
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::InstructionObservationBatchStarted(_)))
    );
}

#[test]
fn an_unavailable_ending_sample_retains_durable_unknown_coverage() {
    let (mut harness, agent, goal, grant) = fixture();
    let (request, sample) = pending(&mut harness, agent, &grant);
    harness.supervisor.instructions.awaiting.remove(&request);
    harness
        .supervisor
        .instructions
        .pending_agents
        .remove(&agent);
    let owner = sample.owner;
    let ticket = sample.ticket;
    let InstructionObservationOwner::Managed { turn, .. } = owner else {
        unreachable!()
    };
    let baseline_lsn = append(
        &mut harness,
        Record::InstructionObservationBatchStarted(sample.clone()),
    );
    let payload = Record::TurnIssued {
        agent,
        turn,
        goal,
        task: "Observe instructions".into(),
        session: None,
        mode: Mode::Managed,
        model_selection: None,
        lsn_of_decision: Lsn(1),
        answers: Vec::new(),
    }
    .issue_payload()
    .unwrap()
    .to_owned();
    append(
        &mut harness,
        Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued { grant, payload })),
    );
    let end_lsn = append(
        &mut harness,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: ExitStatus::UNKNOWN,
            usage: None,
            model_usage: BTreeMap::new(),
            last_message: None,
            session: None,
            outcome: None,
            final_reply: None,
            elapsed_ms: None,
            session_usage: None,
            step_digest: None,
            context: None,
        },
    );
    let baseline = InstructionBaselineRef::Started { lsn: baseline_lsn };
    harness.supervisor.request_instruction_sample(
        owner,
        Some(goal),
        ModelSelection::provider_default(),
        Some(Box::new(InstructionSnapshotEvidence::Repositories(sample))),
        Awaiting::End(Ending {
            owner,
            ticket,
            baseline,
            end: InstructionEndBoundary::ManagedTurn { turn, end_lsn },
            recovery: false,
            then: Box::new(Then::Fold),
        }),
    );
    let request = harness
        .supervisor
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::SampleRepositoryInstructions { request, .. } => Some(*request),
            _ => None,
        })
        .unwrap();
    harness.drive(Input::RepositoryInstructionsUnavailable { request });
    assert!(harness.records().any(|record| matches!(record, Record::InstructionObservationBatchFinished(batch)
        if batch.members.iter().all(|member| member.ending_root.is_none() && matches!(member.settlement,
            InstructionSettlement::Unknown { reason: InstructionUnknownReason::IncompleteCoverage, .. })))));
    assert!(
        harness
            .supervisor
            .world()
            .instructions
            .pending_for_goal(goal)
    );
    assert!(
        !harness
            .supervisor
            .world()
            .instructions
            .repository_turn_covered(turn)
    );
}

#[test]
fn an_unavailable_issued_start_keeps_missing_coverage_without_a_discovery_plan() {
    let (mut harness, agent, goal, grant) = fixture();
    let (request, _) = pending(&mut harness, agent, &grant);
    let mut record = match &harness.supervisor.instructions.awaiting[&request] {
        Awaiting::Start(Starting {
            action: StartAction::Issue { entry, .. },
            ..
        }) => entry.record.clone(),
        _ => unreachable!(),
    };
    let decision = append(
        &mut harness,
        Record::LoopDecided {
            agent,
            input_digest: "retry".into(),
            gate_trace: Vec::new(),
            decision: LoopDecision::Continue {
                task: "Observe instructions".into(),
            },
        },
    );
    let Record::TurnIssuedWithRepositories(issue) = &mut record else {
        unreachable!()
    };
    let dudes_proto::issue::ManagedIssuePayload::Ordinary {
        lsn_of_decision, ..
    } = &mut issue.payload
    else {
        unreachable!()
    };
    *lsn_of_decision = decision;
    let turn = record.turn_issue().unwrap().turn;
    let issue_lsn = append(&mut harness, record);
    let effects = harness.step(Input::RepositoryInstructionsUnavailable { request });
    let entry = effects
        .iter()
        .find_map(|effect| match effect {
            Seen::Persist { entry, .. }
                if matches!(entry.record, Record::InstructionObservationBatchFinished(_)) =>
            {
                Some(entry.clone())
            }
            _ => None,
        })
        .unwrap();
    harness.append_replayed(entry);
    let attempt = harness
        .supervisor
        .world()
        .instructions
        .attempt(issue_lsn)
        .unwrap();
    assert_eq!(
        attempt.baseline,
        InstructionBaselineRef::MissingIssueBoundary { turn, issue_lsn }
    );
    assert!(attempt.snapshot.is_none());
    assert!(attempt.coverage_unknown());
    assert!(attempt.repository_binding().is_some());
    assert!(
        harness
            .supervisor
            .world()
            .instructions
            .pending_for_goal(goal)
    );
    assert!(
        !harness
            .supervisor
            .world()
            .instructions
            .repository_turn_covered(turn)
    );
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::InstructionObservationBatchStarted(_)))
    );
}
