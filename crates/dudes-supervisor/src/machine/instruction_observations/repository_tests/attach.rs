//! An attached session of an unconfined hire opens in its primary copy with the other copies
//! added, and a confined hire is not attached.

use super::*;
use crate::Command;
use dudes_proto::PeerProvenance;
use dudes_types::{SessionId, TurnId};
use std::ffi::OsString;

const NAMES: [&str; 3] = ["web", "api", "docs"];

fn catalog_origin(index: u8) -> TrustedRepository {
    let root = std::env::temp_dir().join(format!("attach-origin-{index}"));
    TrustedRepository {
        common_dir: root.join(".git"),
        directory: root,
        directory_identity: FileIdentity {
            device: 1,
            inode: 10 + u64::from(index),
        },
        common_identity: FileIdentity {
            device: 1,
            inode: 50 + u64::from(index),
        },
    }
}

/// A hire of three copies whose primary is the middle one, replayed into a harness.
#[expect(
    clippy::too_many_lines,
    reason = "one replayed catalog, trust, lease and hire prefix supplies the attach controls"
)]
fn hired_with_three_copies(policy: RepositoryPolicy) -> (Harness, AgentId) {
    let mut harness = Harness::new();
    let ids: Vec<RepoId> = NAMES
        .iter()
        .map(|name| RepoId::parse(name).unwrap())
        .collect();
    let origins: Vec<_> = (0..3).map(catalog_origin).collect();
    let catalog = append(
        &mut harness,
        Record::ProjectRepositoriesRecorded {
            project: "service".into(),
            repositories: RepositorySet::new(
                ids[1].clone(),
                ids.iter()
                    .zip(&origins)
                    .map(|(repo_id, origin)| RepositoryEntry {
                        repo_id: repo_id.clone(),
                        value: CatalogRepository {
                            label: repo_id.as_str().into(),
                            origin: origin.clone(),
                        },
                    })
                    .collect(),
            )
            .unwrap(),
        },
    );
    let base = GitCommit::parse("0000000000000000000000000000000000000000").unwrap();
    let trusts: Vec<_> = ids
        .iter()
        .zip(&origins)
        .map(|(repo_id, origin)| {
            append(
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
            )
        })
        .collect();
    let mut owned = Vec::new();
    for (index, (repo_id, origin)) in ids.iter().zip(&origins).enumerate() {
        let trust = trusts[index];
        let index = u8::try_from(index).unwrap();
        let worktree = WorktreeId::from_bytes([3 + index; 16]);
        let path = std::env::temp_dir().join(format!("attach-copy-{index}"));
        append(
            &mut harness,
            Record::WorktreeLeased {
                worktree,
                origin: origin.directory.clone(),
                path: path.clone(),
                branch: format!("dudes/{index:02}"),
                base,
                from: None,
            },
        );
        owned.push(RepositoryEntry {
            repo_id: repo_id.clone(),
            value: OwnedRepository {
                origin: origin.clone(),
                execution: ExecutionRepository {
                    directory: path,
                    directory_identity: FileIdentity {
                        device: 1,
                        inode: 20 + u64::from(index),
                    },
                    git_dir: origin.common_dir.join(format!("worktrees/copy{index}")),
                    git_identity: FileIdentity {
                        device: 1,
                        inode: 30 + u64::from(index),
                    },
                },
                worktree,
                base,
                trust_lsn: trust,
            },
        });
    }
    let agent = AgentId::from_parts(1, 1);
    let hire = RepositoryHire {
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
        isolation: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
        repositories: ScopedRepositories {
            project: RecordedWorkspace {
                id: "service".into(),
                label: "Service".into(),
            },
            catalog_lsn: catalog,
            policy,
            repositories: RepositorySet::new(ids[1].clone(), owned).unwrap(),
        },
    };
    append(&mut harness, Box::new(hire).into_record());
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hired, true)]));
    append(
        &mut harness,
        Record::SessionLearned {
            agent,
            turn: TurnId::NIL,
            session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
        },
    );
    (harness, agent)
}

fn attach(harness: &mut Harness, agent: AgentId) {
    harness.drive(Input::Command(Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }));
}

fn opened(harness: &Harness) -> Option<dudes_providers::launch::LaunchSpec> {
    harness.trace.iter().find_map(|seen| match seen {
        Seen::OpenPty { spec, .. } => Some(spec.clone()),
        _ => None,
    })
}

#[test]
fn an_unconfined_attach_opens_the_primary_copy_and_adds_each_other_copy() {
    let (mut harness, agent) = hired_with_three_copies(RepositoryPolicy::OperatorTrustV1);
    attach(&mut harness, agent);
    let spec = opened(&harness).expect("the PTY opens once the attach is durable");
    let copies: Vec<_> = (0..3)
        .map(|index| std::env::temp_dir().join(format!("attach-copy-{index}")))
        .collect();
    assert_eq!(
        spec.cwd, copies[1],
        "the primary copy is the working folder"
    );
    let added: Vec<_> = spec
        .argv
        .windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| std::path::PathBuf::from(&pair[1]))
        .collect();
    assert_eq!(added, [copies[0].clone(), copies[2].clone()]);
    assert_eq!(
        spec.argv[1..3],
        [
            OsString::from("--resume"),
            OsString::from("550e8400-e29b-41d4-a716-446655440000")
        ]
    );
}

#[test]
fn a_confined_multi_repository_hire_is_not_attached() {
    let (mut harness, agent) = hired_with_three_copies(RepositoryPolicy::WorkingTreeV1);
    attach(&mut harness, agent);
    assert!(opened(&harness).is_none());
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::Attached { .. }))
    );
}

#[test]
fn a_stop_in_an_unconfined_attached_session_settles_every_copy_with_a_record_the_journal_accepts() {
    use crate::machine::{EventKind, HookId};

    let (mut harness, agent) = hired_with_three_copies(RepositoryPolicy::OperatorTrustV1);
    attach(&mut harness, agent);
    let event = dudes_providers::claude::translate(
        agent,
        Timestamp::UNIX_EPOCH,
        &serde_json::json!({"hook_event_name":"Stop","last_assistant_message":"owned observed end"}),
    )
    .unwrap();
    assert!(!matches!(event.kind, EventKind::PromptSubmit));
    harness.drive(Input::Hook {
        hook: HookId(91),
        event,
    });
    let settled: Vec<_> = harness
        .records()
        .filter_map(|record| match record {
            Record::InstructionObservationBatchFinished(batch) => Some(batch),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), 1);
    let batch = settled[0];
    assert!(matches!(
        batch.owner,
        InstructionObservationOwner::AttachedTurn { .. }
    ));
    assert!(matches!(
        batch.baseline,
        InstructionBaselineRef::MissingTurnBoundary { .. }
    ));
    assert_eq!(
        batch
            .members
            .iter()
            .map(|member| member.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::InstructionObservationFinished { .. })),
        "a multi-repository hire never records the single-folder shape"
    );
}

fn unsampled_member(name: &str, agent: AgentId) -> RepositoryInstructionSnapshot {
    RepositoryInstructionSnapshot {
        repo_id: RepoId::parse(name).unwrap(),
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
    }
}

#[test]
fn a_restart_settles_a_closed_unconfined_attached_session_as_unknown_for_every_copy() {
    use dudes_proto::rpc::pty::PtyId;
    use dudes_proto::{InstructionUnknownReason, PtyCloseOutcome};

    let (mut harness, agent) = hired_with_three_copies(RepositoryPolicy::OperatorTrustV1);
    attach(&mut harness, agent);
    let attachment_lsn = harness
        .supervisor
        .world()
        .instructions
        .attachment_lsn(agent)
        .unwrap();
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;
    let owner = InstructionObservationOwner::AttachedProcess {
        agent,
        attachment_lsn,
    };
    let started = append(
        &mut harness,
        Record::InstructionObservationBatchStarted(RepositoryInstructionStart {
            v: INSTRUCTION_BATCH_VERSION,
            binding: RepositoryInstructionBinding {
                hired_lsn: hired,
                ticket_start_lsn: None,
            },
            owner,
            ticket: None,
            members: NAMES.map(|name| unsampled_member(name, agent)).to_vec(),
        }),
    );
    let pty = PtyId(5);
    append(
        &mut harness,
        Record::PtySpawned {
            agent,
            pty,
            pid: 77,
            start_time: 9,
            boot: None,
        },
    );
    let request_lsn = append(
        &mut harness,
        Record::PtyCloseRequested {
            pty,
            actor: Actor::System,
            provenance: None,
            reason: "test".into(),
        },
    );
    append(
        &mut harness,
        Record::PtyCloseCompleted {
            request_lsn,
            outcome: PtyCloseOutcome::Closed,
        },
    );

    let mut restarted = Harness::from_prefix(harness.journal.clone());
    restarted
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hired, true)]));
    restarted.drive(Input::Started);
    let settled: Vec<_> = restarted
        .records()
        .filter_map(|record| match record {
            Record::InstructionObservationBatchFinished(batch) => Some(batch),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].owner, owner);
    assert_eq!(
        settled[0].baseline,
        InstructionBaselineRef::Started { lsn: started }
    );
    assert_eq!(settled[0].members.len(), 3);
    assert!(settled[0].members.iter().all(|member| matches!(
        member.settlement,
        InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::IncompleteCoverage,
            ..
        }
    )));
    assert!(
        !restarted
            .records()
            .any(|record| matches!(record, Record::InstructionObservationFinished { .. }))
    );
}
