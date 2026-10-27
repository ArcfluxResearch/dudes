use super::*;
use crate::record::samples;
use crate::{Entry, RecordKind};
use dudes_types::Timestamp;

#[test]
fn eight_maximal_members_fit_the_member_and_complete_entry_envelopes() {
    let Record::TicketBaselineBatchRecorded(mut baseline) =
        samples::record(RecordKind::TicketBaselineBatchRecorded)
    else {
        unreachable!()
    };
    let mut member = baseline.members[0].clone();
    member.interval = CaptureInterval {
        started_at: Timestamp::from_unix_millis(i64::MIN),
        ended_at: Timestamp::from_unix_millis(i64::MAX),
    };
    member.versions = SamplerVersions {
        sampler: u32::MAX,
        policy: u32::MAX,
    };
    member.outcome = BaselineOutcome::Captured {
        device: u64::MAX,
        inode: u64::MAX,
        head: dudes_types::GitCommit::parse(&"f".repeat(40)).unwrap(),
        preexisting_tracked: true,
        preexisting_untracked: true,
    };
    baseline.members = (0..MAX_REVIEW_REPOSITORIES)
        .map(|i| RepositoryBaselineMember {
            repo_id: RepoId::parse(&format!("r{i}{}", "x".repeat(30))).unwrap(),
            ..member.clone()
        })
        .collect();
    baseline.binding.ticket.round = u32::MAX;
    baseline.binding.hired_lsn = Lsn(u64::MAX - 4);
    baseline.binding.start_lsn = Lsn(u64::MAX - 3);
    baseline.validate().unwrap();
    for member in &baseline.members {
        assert!(serde_json::to_vec(member).unwrap().len() <= MAX_STAMP_PAYLOAD_BYTES);
    }
    let stamp = RepositoryStampBatch {
        v: REVIEW_BATCH_VERSION,
        binding: baseline.binding,
        baseline_lsn: Lsn(u64::MAX - 2),
        check_lsn: Lsn(u64::MAX - 1),
        members: baseline
            .members
            .iter()
            .map(|member| RepositoryStampMember {
                repo_id: member.repo_id.clone(),
                interval: member.interval,
                versions: member.versions,
                outcome: RepositoryStampOutcome::Sampled {
                    root: FileIdentity {
                        device: u64::MAX,
                        inode: u64::MAX,
                    },
                    complete: false,
                    digest: DigestHex::parse(&"f".repeat(64)).unwrap(),
                    omitted: u32::MAX,
                    omitted_lower_bound: false,
                    untracked: u32::MAX,
                    untracked_lower_bound: false,
                },
            })
            .collect(),
    };
    stamp.validate().unwrap();
    for member in &stamp.members {
        assert!(serde_json::to_vec(member).unwrap().len() <= MAX_STAMP_PAYLOAD_BYTES);
    }
    for record in [
        Record::TicketBaselineBatchRecorded(baseline.clone()),
        Record::TicketReviewBatchStamped(stamp),
    ] {
        let entry = Entry {
            at: Timestamp::from_unix_millis(i64::MIN),
            record,
        };
        assert!(serde_json::to_vec(&entry).unwrap().len() <= MAX_REVIEW_BATCH_BYTES);
    }
    baseline.members.push(RepositoryBaselineMember {
        repo_id: RepoId::parse("overflow").unwrap(),
        ..member
    });
    assert_eq!(baseline.validate(), Err(RepositoryStateError::Review));
}

#[test]
fn an_empty_or_repeated_membership_is_never_complete_evidence() {
    let Record::TicketBaselineBatchRecorded(mut baseline) =
        samples::record(RecordKind::TicketBaselineBatchRecorded)
    else {
        unreachable!()
    };
    baseline.members.push(baseline.members[0].clone());
    assert_eq!(baseline.validate(), Err(RepositoryStateError::Review));
    baseline.members.clear();
    assert_eq!(baseline.validate(), Err(RepositoryStateError::Review));
    assert!(!baseline.complete());
}
