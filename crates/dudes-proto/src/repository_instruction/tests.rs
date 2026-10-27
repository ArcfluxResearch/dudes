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
        let mut unknown = encoded.clone();
        unknown["future_authority"] = true.into();
        assert!(serde_json::from_value::<Entry>(unknown).is_err());
        let mut omitted = encoded;
        omitted.as_object_mut().unwrap().remove("ticket");
        assert!(serde_json::from_value::<Entry>(omitted).is_err());
    }
}
