use super::*;
use crate::instruction_sample::tests::fixture;
use dudes_proto::instruction::{
    InstructionCandidate, InstructionFingerprint, InstructionImportPolicy,
    InstructionIssueAssociation, InstructionSharedInput, MAX_INSTRUCTION_METADATA_BYTES,
    MAX_INSTRUCTION_SOURCE_BYTES, NativeInstructionPath,
};
use dudes_proto::repository::RepoId;
use dudes_proto::repository_instruction::{
    INSTRUCTION_BATCH_VERSION, RepositoryInstructionBinding, RepositoryInstructionSnapshot,
};
use dudes_types::TurnId;
use std::fs;
use std::path::{Path, PathBuf};

fn seed(paths: &[PathBuf]) -> RepositoryInstructionStart {
    RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding: RepositoryInstructionBinding {
            hired_lsn: Lsn(1),
            ticket_start_lsn: None,
        },
        owner: InstructionObservationOwner::Managed {
            agent: AgentId::from_parts(1, 1),
            turn: TurnId::from_parts(1, 2),
            issue: InstructionIssueAssociation::Prospective,
        },
        ticket: None,
        members: paths
            .iter()
            .enumerate()
            .map(|(index, path)| RepositoryInstructionSnapshot {
                repo_id: RepoId::parse(&format!("repo{index}")).unwrap(),
                root: Some(crate::worktree::trust::identity(path).unwrap()),
                snapshot: fixture(path),
            })
            .collect(),
    }
}

fn directories(root: &Path) -> Vec<PathBuf> {
    ["web", "api", "docs"]
        .map(|name| {
            let path = root.join(name);
            fs::create_dir(&path).unwrap();
            path.canonicalize().unwrap()
        })
        .into()
}

fn observed(sample: RepositoryInstructionStart, starting: bool) -> RepositoryInstructionStart {
    let progress = Arc::new(Mutex::new(Progress {
        sample: sample.clone(),
        completed: 0,
    }));
    observe_reporting(
        sample,
        &progress,
        Instant::now() + crate::instruction_sample::SAMPLE_DEADLINE,
        starting,
    )
}

#[test]
fn all_members_keep_order_and_external_import_provenance() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = directories(temporary.path());
    let external = temporary.path().join("profile.md");
    fs::write(&external, b"profile instructions").unwrap();
    let mut plan = seed(&paths);
    for member in &mut plan.members {
        member.snapshot.participants[0].discovery.rules[0].imports =
            InstructionImportPolicy::ClaudeFourHops;
    }
    for path in &paths {
        fs::write(path.join("CLAUDE.md"), b"@../profile.md").unwrap();
    }
    let before = observed(plan.clone(), true);
    fs::write(&external, b"new profile instructions").unwrap();
    fs::write(paths[2].join("AGENTS.md"), b"third repository changed").unwrap();
    let after = observed(plan, false);
    assert_eq!(
        after
            .members
            .iter()
            .map(|member| member.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["repo0", "repo1", "repo2"]
    );
    let comparisons = compare(&before, &after);
    let canonical = NativeInstructionPath::from_path(&external.canonicalize().unwrap()).unwrap();
    for (index, member) in after.members.iter().enumerate() {
        assert_eq!(
            member.root,
            Some(crate::worktree::trust::identity(&paths[index]).unwrap())
        );
        let import = member
            .snapshot
            .inputs
            .iter()
            .find(|input| input.resolved_path.as_ref() == Some(&canonical))
            .unwrap();
        assert_eq!(import.source.rule, 0);
        assert!(matches!(
            import.context,
            InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Import
            }
        ));
        assert!(
            comparisons[index]
                .comparison
                .changes
                .iter()
                .any(|change| change.path == import.path)
        );
    }
    assert_eq!(comparisons[2].comparison.changes.len(), 2);
}

#[test]
fn source_byte_limit_is_shared_across_members() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = directories(temporary.path());
    let bytes = vec![b'x'; (MAX_INSTRUCTION_SOURCE_BYTES * 3 / 4) as usize];
    for path in &paths {
        fs::write(path.join("CLAUDE.md"), &bytes).unwrap();
    }
    let sample = observed(seed(&paths), true);
    assert!(sample.validate().is_ok());
    assert_eq!(
        sample.members[0].snapshot.usage.source_bytes,
        bytes.len() as u64
    );
    for member in &sample.members[1..] {
        assert_eq!(member.snapshot.usage.source_bytes, 0);
        assert!(
            member
                .snapshot
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::SourceBytesLimit)
        );
        assert!(
            !member
                .snapshot
                .inputs
                .iter()
                .any(|input| matches!(input.fingerprint, InstructionFingerprint::File { .. }))
        );
    }
}

#[test]
fn metadata_saturation_preserves_members_and_unknown_coverage() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = directories(temporary.path());
    let mut plan = seed(&paths);
    for member in &mut plan.members {
        member.snapshot.participants[0].discovery.rules[0].candidate =
            InstructionCandidate::Fixed {
                paths: (0..64)
                    .map(|index| {
                        NativeInstructionPath::from_path(Path::new(&format!(
                            "{index:03}-{}.md",
                            "instruction".repeat(8)
                        )))
                        .unwrap()
                    })
                    .collect(),
            };
    }
    assert!(seed_fits(&plan));
    let sample = observed(plan, true);
    assert!(sample.validate().is_ok());
    assert_eq!(sample.members.len(), 3);
    assert!(sample.members.iter().any(|member| {
        member
            .snapshot
            .gaps
            .iter()
            .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
    }));
    let expired = deadline(
        &Mutex::new(Progress {
            sample: sample.clone(),
            completed: 2,
        }),
        Timestamp::UNIX_EPOCH,
    );
    assert!(expired.validate().is_ok());
    let record = dudes_proto::Entry {
        at: Timestamp::from_unix_millis(i64::MIN),
        record: dudes_proto::Record::InstructionObservationBatchStarted(sample),
    };
    assert!(serde_json::to_vec(&record).unwrap().len() <= MAX_INSTRUCTION_METADATA_BYTES);
}

#[test]
fn deadline_keeps_known_changes_and_clears_unfinished_root_claims() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = directories(temporary.path());
    fs::write(paths[0].join("CLAUDE.md"), b"before").unwrap();
    let plan = seed(&paths);
    let before = observed(plan.clone(), true);
    fs::write(paths[0].join("CLAUDE.md"), b"after").unwrap();
    let mut partial = plan;
    partial.members[0].snapshot =
        crate::instruction_sample::observe(partial.members[0].snapshot.clone());
    let progress = Mutex::new(Progress {
        sample: partial,
        completed: 1,
    });
    let after = deadline(&progress, Timestamp::UNIX_EPOCH);
    assert!(after.validate().is_ok(), "{:?}", after.validate());
    assert_eq!(after.members[0].root, before.members[0].root);
    for member in &after.members[1..] {
        assert!(member.root.is_none());
        assert!(member.snapshot.execution_folder.is_none());
        assert!(
            member
                .snapshot
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::Deadline)
        );
    }
    let comparisons = compare(&before, &after);
    assert_eq!(comparisons[0].comparison.changes.len(), 1);
    assert!(
        comparisons[1..]
            .iter()
            .all(|member| !member.comparison.gaps.is_empty())
    );
}

#[test]
fn replacement_root_never_inherits_the_original_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = directories(temporary.path());
    fs::write(paths[2].join("CLAUDE.md"), b"before").unwrap();
    let plan = seed(&paths);
    let before = observed(plan.clone(), true);
    fs::rename(&paths[2], temporary.path().join("retained")).unwrap();
    fs::create_dir(&paths[2]).unwrap();
    fs::write(paths[2].join("CLAUDE.md"), b"replacement").unwrap();
    let after = observed(plan.clone(), false);
    assert_ne!(after.members[2].root, before.members[2].root);
    assert_eq!(
        after.members[2].root,
        Some(crate::worktree::trust::identity(&paths[2]).unwrap())
    );
    let comparison = &compare(&before, &after)[2].comparison;
    assert_eq!(comparison.changes.len(), 1);
    assert!(
        comparison
            .gaps
            .iter()
            .any(|gap| gap.reason == InstructionGapKind::ChangedDuringRead)
    );
    let start = observed(plan, true);
    assert!(start.members[2].root.is_none());
    assert!(start.members[2].snapshot.execution_folder.is_none());
}
