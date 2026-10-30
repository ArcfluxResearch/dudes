use super::*;
use crate::instruction::{
    InstructionCoverageGap, InstructionGapKind, InstructionPathContext,
    MAX_INSTRUCTION_SOURCE_BYTES, NativeInstructionPath,
};
use crate::record::{Entry, RecordKind, samples};
use dudes_types::Timestamp;

fn start() -> RepositoryInstructionStart {
    let Record::InstructionObservationBatchStarted(mut batch) =
        samples::record(RecordKind::InstructionObservationBatchStarted)
    else {
        unreachable!()
    };
    let mut second = batch.members[0].clone();
    second.repo_id = RepoId::parse("other").unwrap();
    batch.members.push(second);
    batch
}

#[test]
fn work_and_participation_caps_are_one_budget_for_the_whole_hire() {
    let mut batch = start();
    for member in &mut batch.members {
        member.snapshot.usage.source_bytes = MAX_INSTRUCTION_SOURCE_BYTES / 2;
        member.snapshot.usage.ancestor_steps = 32;
        member.snapshot.usage.descendant_entries = 2_048;
        member.snapshot.usage.import_edges = 256;
        member.snapshot.usage.descendant_depth = 16;
        member.snapshot.usage.symlink_hops = 8;
    }
    assert_eq!(batch.validate(), Ok(()));
    batch.members[1].snapshot.usage.source_bytes += 1;
    assert_eq!(batch.validate(), Err(InstructionValidationError::WorkLimit));
    batch.members[1].snapshot.usage.source_bytes -= 1;
    batch.members[1].snapshot.usage.ancestor_steps += 1;
    assert_eq!(batch.validate(), Err(InstructionValidationError::WorkLimit));
    let capped = InstructionSampleUsage {
        source_bytes: u64::MAX,
        ..InstructionSampleUsage::default()
    };
    assert_eq!(
        capped.checked_add(capped),
        Err(InstructionValidationError::WorkLimit)
    );
    let mut batch = start();
    let participant = crate::InstructionParticipant {
        agent: samples::agent(),
        provider: dudes_types::Provider::Claude,
        mode: crate::InstructionDeclarationMode::Unknown,
        declaration_digest: crate::record::Hex32::new([1; 32]),
        discovery: crate::InstructionDiscoveryPlan {
            roots: Vec::new(),
            rules: Vec::new(),
            configuration_digest: None,
            gaps: batch.members[0].snapshot.gaps.clone(),
        },
    };
    for member in &mut batch.members {
        member.snapshot.participants = vec![participant.clone(); 17];
        member.snapshot.validate().unwrap();
    }
    assert_eq!(batch.validate(), Err(InstructionValidationError::WorkLimit));
}

#[test]
fn encoded_metadata_counts_repository_and_carrier_overhead_once() {
    let mut batch = start();
    for member in &mut batch.members {
        member.snapshot.gaps = vec![
            InstructionCoverageGap {
                reason: InstructionGapKind::ReadFailed,
                context: InstructionPathContext::Unresolved,
                agent: None,
                path: Some(NativeInstructionPath::Utf8 {
                    value: format!("/{}", "x".repeat(3_800))
                }),
                origin: crate::InstructionGapOrigin::Observed,
            };
            9
        ];
        assert!(member.snapshot.validate().is_ok());
    }
    assert_eq!(
        batch.validate(),
        Err(InstructionValidationError::MetadataTooLarge)
    );
    let record = Record::InstructionObservationBatchStarted(batch);
    assert_eq!(
        crate::validate_instruction_record(&record),
        Err(InstructionValidationError::MetadataTooLarge)
    );
}

#[test]
fn new_carriers_keep_exact_required_tags_and_closed_entry_schemas() {
    let schema = crate::schema::schema_for::<Entry>().to_value();
    for (kind, tag) in [
        (RecordKind::InstructionObservationBatchStarted, 110),
        (RecordKind::InstructionObservationBatchFinished, 111),
    ] {
        assert_eq!(kind.tag(), tag);
        assert!(kind.requires_required_frame());
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(kind),
        };
        crate::validate_instruction_record(&entry.record).unwrap();
        let encoded = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            serde_json::from_value::<Entry>(encoded.clone()).unwrap(),
            entry
        );
        let variant = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["t"]["const"] == encoded["t"])
            .unwrap();
        assert_eq!(variant["additionalProperties"], false);
        if let Record::InstructionObservationBatchFinished(mut finish) = entry.record.clone() {
            let mut absent_root = encoded.clone();
            absent_root["members"][0]
                .as_object_mut()
                .unwrap()
                .remove("ending_root");
            assert!(serde_json::from_value::<Entry>(absent_root).is_err());
            finish.members[0].ending_root = Some(FileIdentity {
                device: 1,
                inode: 2,
            });
            assert_eq!(
                finish.validate(),
                Err(InstructionValidationError::InvalidBoundary)
            );
        }
        let mut unknown = encoded.clone();
        unknown["future_authority"] = true.into();
        assert!(serde_json::from_value::<Entry>(unknown).is_err());
        let mut omitted = encoded;
        omitted.as_object_mut().unwrap().remove("ticket");
        assert!(serde_json::from_value::<Entry>(omitted).is_err());
    }
}

mod attached_owner {
    use super::*;
    use crate::instruction::{InstructionBaselineRef, InstructionObservationOwner};
    use crate::repository::{HireRepositories, RepositoryPolicy};
    use dudes_types::{AgentId, Lsn, TurnId};

    const REFUSED: Result<(), InstructionValidationError> =
        Err(InstructionValidationError::InvalidBoundary);

    fn hire_with(policy: RepositoryPolicy) -> HireRepositories {
        let Record::AgentHiredWithRepositories(hire) =
            samples::record(RecordKind::AgentHiredWithRepositories)
        else {
            unreachable!()
        };
        let sample = hire.repositories;
        let pin = crate::record::TrustPin {
            claude_tree: None,
            mcp_json: None,
            attributes: crate::record::Hex32::new([7; 32]),
            config: crate::record::Hex32::new([8; 32]),
        };
        let entries = sample
            .repositories
            .entries()
            .iter()
            .map(|entry| crate::repository::RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: crate::repository::HiredRepository::new(entry.value.clone(), pin.clone()),
            })
            .collect();
        HireRepositories::Scoped(crate::repository::ScopedHireRepositories {
            project: sample.project.clone(),
            catalog_lsn: sample.catalog_lsn,
            policy,
            repositories: crate::repository::RepositorySet::new(
                sample.repositories.primary_id().clone(),
                entries,
            )
            .unwrap(),
        })
    }

    fn hired_lsn() -> Lsn {
        Lsn::from_parts(1, 256).unwrap()
    }

    fn facts(hire: &HireRepositories) -> RepositoryInstructionFacts<'_> {
        RepositoryInstructionFacts {
            agent: samples::agent(),
            hire: Some((hired_lsn(), hire)),
            round: None,
            issue: None,
        }
    }

    fn process(agent: AgentId) -> InstructionObservationOwner {
        InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn: Lsn::from_parts(1, 300).unwrap(),
        }
    }

    fn stopped_turn(agent: AgentId) -> InstructionObservationOwner {
        InstructionObservationOwner::AttachedTurn {
            agent,
            turn: TurnId::NIL,
            attachment_lsn: None,
        }
    }

    fn started(owner: InstructionObservationOwner, hire: &HireRepositories) -> Record {
        let Record::InstructionObservationBatchStarted(mut batch) =
            samples::record(RecordKind::InstructionObservationBatchStarted)
        else {
            unreachable!()
        };
        let HireRepositories::Scoped(hire) = hire else {
            unreachable!()
        };
        let copy = &hire.repositories.entries()[0].value.recorded.execution;
        batch.owner = owner;
        batch.members[0].root = Some(copy.directory_identity);
        batch.members[0].snapshot.execution_folder =
            Some(NativeInstructionPath::from_path(&copy.directory).unwrap());
        let lease = hire.repositories.entries()[0].value.recorded.worktree;
        let snapshot = &mut batch.members[0].snapshot;
        for context in snapshot
            .inputs
            .iter_mut()
            .map(|input| &mut input.context)
            .chain(snapshot.gaps.iter_mut().map(|gap| &mut gap.context))
        {
            if matches!(context, InstructionPathContext::OwnedWorktree { .. }) {
                *context = InstructionPathContext::OwnedWorktree { lease };
            }
        }
        Record::InstructionObservationBatchStarted(batch)
    }

    fn finished(owner: InstructionObservationOwner, baseline: InstructionBaselineRef) -> Record {
        let Record::InstructionObservationBatchFinished(mut batch) =
            samples::record(RecordKind::InstructionObservationBatchFinished)
        else {
            unreachable!()
        };
        batch.settlement_for(owner, baseline);
        Record::InstructionObservationBatchFinished(batch)
    }

    trait Settle {
        fn settlement_for(
            &mut self,
            owner: InstructionObservationOwner,
            baseline: InstructionBaselineRef,
        );
    }

    impl Settle for RepositoryInstructionFinish {
        fn settlement_for(
            &mut self,
            owner: InstructionObservationOwner,
            baseline: InstructionBaselineRef,
        ) {
            let settlement = match (owner, baseline) {
                (
                    InstructionObservationOwner::AttachedTurn { turn, .. },
                    InstructionBaselineRef::MissingTurnBoundary { end_lsn, .. },
                ) => {
                    let mut comparison = crate::InstructionComparison::default();
                    assert!(comparison.try_push_gap(InstructionCoverageGap {
                        origin: crate::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::AttachedTurnStartMissing,
                        context: InstructionPathContext::Unresolved,
                        agent: None,
                        path: None,
                    }));
                    InstructionSettlement::Unknown {
                        reason: crate::InstructionUnknownReason::AttachedTurnStartMissing,
                        end: Some(crate::InstructionEndBoundary::AttachedTurn { turn, end_lsn }),
                        sampled_at: None,
                        recovery: false,
                        comparison,
                    }
                }
                (InstructionObservationOwner::AttachedProcess { attachment_lsn, .. }, _) => {
                    InstructionSettlement::Compared {
                        end: crate::InstructionEndBoundary::AttachedProcess {
                            attachment_lsn,
                            end_lsn: Lsn::from_parts(1, 330).unwrap(),
                        },
                        sampled_at: Timestamp::UNIX_EPOCH,
                        recovery: false,
                        comparison: crate::InstructionComparison::default(),
                    }
                }
                _ => unreachable!("only attached owners are built here"),
            };
            self.owner = owner;
            self.baseline = baseline;
            self.members[0].ending_root =
                matches!(settlement, InstructionSettlement::Compared { .. }).then_some(
                    FileIdentity {
                        device: 1,
                        inode: 20,
                    },
                );
            self.members[0].settlement = settlement;
        }
    }

    fn later() -> Lsn {
        Lsn::from_parts(1, 400).unwrap()
    }

    fn missing_turn() -> InstructionBaselineRef {
        InstructionBaselineRef::MissingTurnBoundary {
            turn: TurnId::NIL,
            end_lsn: Lsn::from_parts(1, 350).unwrap(),
        }
    }

    fn open_session() -> InstructionBaselineRef {
        InstructionBaselineRef::Started {
            lsn: Lsn::from_parts(1, 310).unwrap(),
        }
    }

    #[test]
    fn an_unconfined_hire_accepts_the_start_and_both_finishes_of_an_attached_owner() {
        let hire = hire_with(RepositoryPolicy::OperatorTrustV1);
        let agent = samples::agent();
        assert_eq!(
            validate_observation(later(), &started(process(agent), &hire), facts(&hire)),
            Ok(())
        );
        assert_eq!(
            validate_observation(
                later(),
                &finished(process(agent), open_session()),
                facts(&hire)
            ),
            Ok(())
        );
        assert_eq!(
            validate_observation(
                later(),
                &finished(stopped_turn(agent), missing_turn()),
                facts(&hire)
            ),
            Ok(())
        );
    }

    #[test]
    fn a_confined_hire_refuses_every_per_copy_record_of_an_attached_owner() {
        let hire = hire_with(RepositoryPolicy::WorkingTreeV1);
        let agent = samples::agent();
        assert_eq!(
            validate_observation(later(), &started(process(agent), &hire), facts(&hire)),
            REFUSED
        );
        assert_eq!(
            validate_observation(
                later(),
                &finished(process(agent), open_session()),
                facts(&hire)
            ),
            REFUSED
        );
        assert_eq!(
            validate_observation(
                later(),
                &finished(stopped_turn(agent), missing_turn()),
                facts(&hire)
            ),
            REFUSED
        );
    }

    #[test]
    fn a_confined_hire_still_records_its_managed_turns() {
        let hire = hire_with(RepositoryPolicy::WorkingTreeV1);
        let record = samples::record(RecordKind::InstructionObservationBatchFinished);
        let Record::InstructionObservationBatchFinished(batch) = &record else {
            unreachable!()
        };
        assert!(matches!(
            batch.owner,
            InstructionObservationOwner::Managed { .. }
        ));
        assert_eq!(validate_observation(later(), &record, facts(&hire)), Ok(()));
    }

    #[test]
    fn an_attached_turn_never_starts_a_batch() {
        assert_eq!(
            crate::validate_instruction_record(&started(
                stopped_turn(samples::agent()),
                &hire_with(RepositoryPolicy::OperatorTrustV1),
            )),
            REFUSED
        );
    }

    #[test]
    fn a_record_naming_another_agents_attachment_is_refused() {
        let hire = hire_with(RepositoryPolicy::OperatorTrustV1);
        let other = AgentId::from_bytes([9; 16]);
        assert_eq!(
            validate_observation(later(), &started(process(other), &hire), facts(&hire)),
            REFUSED
        );
        assert_eq!(
            validate_observation(
                later(),
                &finished(stopped_turn(other), missing_turn()),
                facts(&hire)
            ),
            REFUSED
        );
    }

    #[test]
    fn a_single_repository_agents_attached_records_are_unchanged() {
        let Record::InstructionObservationStarted {
            ticket, snapshot, ..
        } = samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!()
        };
        let legacy = |owner| Record::InstructionObservationStarted {
            owner,
            ticket,
            snapshot: snapshot.clone(),
        };
        let agent = samples::agent();
        assert_eq!(
            crate::validate_instruction_record(&legacy(process(agent))),
            Ok(())
        );
        assert_eq!(
            crate::validate_instruction_record(&legacy(stopped_turn(agent))),
            REFUSED
        );
    }
}
