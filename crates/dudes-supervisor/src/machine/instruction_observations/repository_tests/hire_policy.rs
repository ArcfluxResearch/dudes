//! An attached session takes the acceptance rule its hire was recorded with, and a hire the state
//! has not seen stays strict.

use super::*;
use dudes_proto::PtyCloseOutcome;
use dudes_proto::instruction_state::InstructionState;
use dudes_proto::rpc::pty::PtyId;

struct Attached {
    harness: Harness,
    goal: GoalId,
    card: CardId,
}

#[expect(
    clippy::too_many_lines,
    reason = "one replayed attach, sample and close chain supplies the attached round"
)]
fn attached_round(extra: Option<fn(AgentId) -> InstructionCoverageGap>) -> Attached {
    let (mut harness, agent, goal, grant) = fixture_for(RepositoryPolicy::OperatorTrustV1);
    let attachment_lsn = append(
        &mut harness,
        Record::Attached {
            agent,
            by: Actor::System,
            hook_port: None,
            provenance: None,
        },
    );
    let pty = PtyId(1);
    append(
        &mut harness,
        Record::PtySpawned {
            agent,
            pty,
            pid: 10,
            start_time: 1,
            boot: None,
        },
    );
    let execution = grant.repositories.entries()[0].value.execution.clone();
    let root = execution.directory_identity;
    let owner = InstructionObservationOwner::AttachedProcess {
        agent,
        attachment_lsn,
    };
    let ticket = harness
        .supervisor
        .world()
        .instructions
        .association_for_goal(goal, Seq(2));
    let standing = |kind| standing_gap(kind, agent);
    let start = RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding: RepositoryInstructionBinding {
            hired_lsn: grant.hired_lsn,
            ticket_start_lsn: grant.ticket.as_ref().map(|round| round.start_lsn),
        },
        ticket,
        owner,
        members: vec![RepositoryInstructionSnapshot {
            repo_id: grant.repositories.primary_id().clone(),
            root: Some(root),
            snapshot: InstructionSnapshot {
                sampled_at: Timestamp::UNIX_EPOCH,
                source: InstructionSnapshotSource::Fresh,
                execution_folder: Some(
                    dudes_proto::NativeInstructionPath::from_path(&execution.directory).unwrap(),
                ),
                participants: Vec::new(),
                inputs: Vec::new(),
                inventoried: Vec::new(),
                gaps: vec![standing(InstructionGapKind::ConfigurationUnknown)],
                usage: InstructionSampleUsage::default(),
                pid_namespace: None,
                boot: None,
            },
        }],
    };
    let baseline_lsn = append(
        &mut harness,
        Record::InstructionObservationBatchStarted(start.clone()),
    );
    let request_lsn = append(
        &mut harness,
        Record::PtyCloseRequested {
            pty,
            actor: Actor::System,
            provenance: None,
            reason: "session ended".into(),
        },
    );
    let end_lsn = append(
        &mut harness,
        Record::PtyCloseCompleted {
            request_lsn,
            outcome: PtyCloseOutcome::Closed,
        },
    );
    append(
        &mut harness,
        Record::InstructionObservationBatchFinished(RepositoryInstructionFinish {
            v: INSTRUCTION_BATCH_VERSION,
            binding: start.binding,
            owner,
            ticket: start.ticket,
            baseline: InstructionBaselineRef::Started { lsn: baseline_lsn },
            members: vec![RepositoryInstructionSettlement {
                repo_id: grant.repositories.primary_id().clone(),
                ending_root: Some(root),
                settlement: InstructionSettlement::Compared {
                    end: InstructionEndBoundary::AttachedProcess {
                        attachment_lsn,
                        end_lsn,
                    },
                    sampled_at: Timestamp::UNIX_EPOCH,
                    recovery: false,
                    comparison: InstructionComparison {
                        changes: Vec::new(),
                        gaps: [
                            standing(InstructionGapKind::ConfigurationUnknown),
                            standing(InstructionGapKind::PluginUnknown),
                            standing(InstructionGapKind::PolicyUnknown),
                        ]
                        .into_iter()
                        .chain(extra.map(|extra| extra(agent)))
                        .collect(),
                        usage: InstructionSampleUsage::default(),
                    },
                },
            }],
        }),
    );
    Attached {
        harness,
        goal,
        card: CardId::from_parts(1, 2),
    }
}

#[test]
fn an_attached_session_of_an_unconfined_hire_that_changes_nothing_holds_no_ticket_or_launch() {
    let round = attached_round(None);
    let state = &round.harness.supervisor.world().instructions;
    assert!(!state.pending_for_goal(round.goal));
    assert_eq!(state.ticket_reason(round.card), None);
    let replayed = Harness::from_prefix(round.harness.journal.clone());
    let state = &replayed.supervisor.world().instructions;
    assert!(!state.pending_for_goal(round.goal));
    assert_eq!(state.ticket_reason(round.card), None);
}

#[test]
fn an_observed_gap_in_an_attached_session_of_an_unconfined_hire_still_holds() {
    let round = attached_round(Some(owned_copy_gap));
    let state = &round.harness.supervisor.world().instructions;
    assert!(state.pending_for_goal(round.goal));
}

fn fold_without(journal: &[Entry], skipped: Option<usize>) -> InstructionState {
    let mut state = InstructionState::default();
    for (index, entry) in journal.iter().enumerate() {
        if Some(index) != skipped {
            state
                .apply_validated(Lsn(index as u64 + 1), entry, |bytes| {
                    dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
                })
                .unwrap();
        }
    }
    state
}

#[test]
fn an_attached_session_whose_hire_the_state_never_saw_stays_strict() {
    let round = attached_round(None);
    let journal = &round.harness.journal;
    let hire = journal
        .iter()
        .position(|entry| entry.record.repository_hire().is_some())
        .unwrap();
    assert!(!fold_without(journal, None).pending_for_goal(round.goal));
    assert!(fold_without(journal, Some(hire)).pending_for_goal(round.goal));
}
