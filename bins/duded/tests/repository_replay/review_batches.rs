use super::*;
use dudes_proto::diff_stamp::{BaselineOutcome, CaptureInterval, DigestHex, SamplerVersions};
use dudes_proto::repository::{RepositoryTurnGrant, RepositoryTurnIssued};
use dudes_proto::repository_review::{
    REVIEW_BATCH_VERSION, RepositoryBaselineBatch, RepositoryBaselineMember,
    RepositoryReviewReadiness, RepositoryStampBatch, RepositoryStampMember, RepositoryStampOutcome,
    TicketReviewState,
};
use dudes_proto::{DiffFreshness, DiffRefusal, Evidence, Gate, GateStep, LoopDecision};
use dudes_types::{GoalId, Provenance, TurnId};

#[path = "review_batches/capture_journey.rs"]
mod capture_journey;
#[path = "review_batches/instruction_batches.rs"]
mod instruction_batches;

struct ReviewLog {
    writer: Writer,
    world: dudes_supervisor::World,
    projection: Projections,
    story: Story,
    goal: GoalId,
}

impl ReviewLog {
    fn new(fixture: &Fixture) -> Self {
        let story = Story::seed(fixture);
        let start = ticket_start(&story);
        let (mut writer, _) =
            dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
        seed_ticket(&mut writer, &start);
        append(
            &mut writer,
            Record::TicketStartedWithRepositories(Box::new(start.clone())),
        );
        drop(writer);
        let (writer, world, projection) = replay_pair(fixture);
        Self {
            writer,
            world,
            projection,
            story,
            goal: start.goal.id,
        }
    }

    fn append(&mut self, record: Record) -> Lsn {
        let recorded = write_record(&mut self.writer, record);
        apply_pair(&mut self.world, &mut self.projection, &recorded);
        self.assert_pair();
        recorded.0
    }

    fn assert_pair(&self) {
        assert_eq!(
            self.projection.goal(self.goal).unwrap().unwrap().review,
            self.world.goals[&self.goal].review
        );
    }

    fn readiness(&self) -> RepositoryReviewReadiness {
        self.world.repository_review(self.goal).unwrap().readiness()
    }

    fn baseline(&self) -> RepositoryBaselineBatch {
        let review = self.world.repository_review(self.goal).unwrap();
        RepositoryBaselineBatch {
            v: REVIEW_BATCH_VERSION,
            binding: review.binding,
            members: review
                .expected
                .iter()
                .map(|repo_id| {
                    let copy = self
                        .story
                        .hire
                        .repositories
                        .repositories
                        .get(repo_id)
                        .unwrap();
                    let root = copy.execution.directory_identity;
                    RepositoryBaselineMember {
                        repo_id: repo_id.clone(),
                        interval: CaptureInterval {
                            started_at: Timestamp::UNIX_EPOCH,
                            ended_at: Timestamp::UNIX_EPOCH,
                        },
                        versions: SamplerVersions {
                            sampler: 1,
                            policy: 1,
                        },
                        outcome: BaselineOutcome::Captured {
                            device: root.device,
                            inode: root.inode,
                            head: copy.base,
                            preexisting_tracked: false,
                            preexisting_untracked: true,
                        },
                    }
                })
                .collect(),
        }
    }

    fn issue(&mut self) -> TurnId {
        let decision = self.append(Record::LoopDecided {
            agent: self.story.hire.id,
            input_digest: "review-work".into(),
            gate_trace: Gate::ORDER
                .into_iter()
                .map(|gate| GateStep {
                    gate,
                    fired: gate == Gate::Work,
                    note: None,
                })
                .collect(),
            decision: LoopDecision::Continue {
                task: "Repair both repositories".into(),
            },
        });
        let turn = TurnId::from_parts(60, 1);
        let record = Record::TurnIssued {
            agent: self.story.hire.id,
            turn,
            goal: self.goal,
            task: "Repair both repositories".into(),
            session: None,
            mode: dudes_types::Mode::Managed,
            model_selection: None,
            lsn_of_decision: decision,
            answers: Vec::new(),
        };
        let grant = RepositoryTurnGrant::select(
            self.story.hired_lsn,
            scoped_hire(&self.world, self.story.hire.id),
            self.world.ticket_repository_round(self.goal),
        )
        .unwrap();
        self.append(Record::TurnIssuedWithRepositories(Box::new(
            RepositoryTurnIssued {
                grant,
                payload: record.issue_payload().unwrap().to_owned(),
            },
        )));
        turn
    }

    fn check(&mut self, turn: Option<TurnId>, pass: bool) -> Lsn {
        self.append(Record::PredicateResult {
            goal: self.goal,
            turn,
            provenance: Provenance::Observed,
            pass,
            evidence: Evidence::Command {
                argv: vec!["/usr/bin/true".into()],
                program: Some("/usr/bin/true".into()),
                exit: Some(i32::from(!pass)),
                output_tail: String::new(),
            },
        })
    }

    fn stamp(&self, baseline_lsn: Lsn, check_lsn: Lsn) -> RepositoryStampBatch {
        let baseline = self.baseline();
        RepositoryStampBatch {
            v: REVIEW_BATCH_VERSION,
            binding: baseline.binding,
            baseline_lsn,
            check_lsn,
            members: baseline
                .members
                .iter()
                .map(|member| RepositoryStampMember {
                    repo_id: member.repo_id.clone(),
                    interval: member.interval,
                    versions: member.versions,
                    outcome: RepositoryStampOutcome::Sampled {
                        root: self
                            .story
                            .hire
                            .repositories
                            .repositories
                            .get(&member.repo_id)
                            .unwrap()
                            .execution
                            .directory_identity,
                        complete: true,
                        digest: DigestHex::parse(&"ab".repeat(32)).unwrap(),
                        omitted: 0,
                        omitted_lower_bound: false,
                        untracked: 0,
                        untracked_lower_bound: false,
                    },
                })
                .collect(),
        }
    }

    fn refuses(&mut self, record: Record) {
        let before = self.projection.export().unwrap();
        let review = self.world.goals[&self.goal].review.clone();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        };
        let lsn = Lsn(self.projection.last_applied_lsn().0 + 1);
        assert!(self.world.apply_checked(lsn, &entry).is_err());
        assert!(matches!(
            self.projection.apply(lsn, 1, &entry),
            Err(dudes_journal::Error::Repository { .. })
        ));
        assert_eq!(self.world.goals[&self.goal].review, review);
        assert_eq!(self.projection.export().unwrap(), before);
    }
}

#[test]
fn complete_two_of_three_batches_survive_reopen_and_rebuild_with_unknown_freshness_preserved() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let complete = log.baseline();
    assert_eq!(
        complete
            .members
            .iter()
            .map(|m| m.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api"]
    );
    let mut incomplete = complete.clone();
    incomplete.members[1].outcome = BaselineOutcome::Unavailable {
        reason: DiffRefusal::TimedOut,
    };
    log.append(Record::TicketBaselineBatchRecorded(incomplete));
    assert_eq!(
        log.readiness(),
        RepositoryReviewReadiness::IncompleteBaseline
    );
    let baseline_lsn = log.append(Record::TicketBaselineBatchRecorded(complete));
    let turn = log.issue();
    let check_lsn = log.check(Some(turn), true);
    let stamp = log.stamp(baseline_lsn, check_lsn);
    let mut unknown = stamp.clone();
    unknown.members[1].outcome = RepositoryStampOutcome::Unavailable {
        reason: DiffRefusal::Unreadable,
    };
    log.append(Record::TicketReviewBatchStamped(unknown.clone()));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::UnknownStamp);
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&unknown),
        DiffFreshness::Unknown
    );
    log.append(Record::TicketReviewBatchStamped(stamp.clone()));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::Complete);
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&stamp),
        DiffFreshness::MatchesReviewSample
    );
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&unknown),
        DiffFreshness::Unknown
    );
    let mut missing = stamp.clone();
    missing.members.pop();
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&missing),
        DiffFreshness::Unknown
    );
    let mut changed = stamp.clone();
    if let RepositoryStampOutcome::Sampled { digest, .. } = &mut changed.members[0].outcome {
        *digest = DigestHex::parse(&"cd".repeat(32)).unwrap();
    }
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&changed),
        DiffFreshness::ChangedAfterCheck
    );
    let expected = log.projection.export().unwrap();
    assert!(expected.contains("\"review\":{\"kind\":\"scoped\""));
    let expected_review = log.world.goals[&log.goal].review.clone();
    let goal = log.goal;
    drop(log);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut cold = Projections::in_memory().unwrap();
    replay(&reader, &mut cold).unwrap();
    assert_eq!(cold.export().unwrap(), expected);
    let mut rebuilt = dudes_supervisor::World::default();
    for item in reader.iter_from(Lsn::ZERO).unwrap() {
        let (lsn, entry) = item.unwrap();
        rebuilt.apply_checked(lsn, &entry).unwrap();
    }
    assert_eq!(rebuilt.goals[&goal].review, expected_review);
    assert!(matches!(
        cold.goal(goal).unwrap().unwrap().review,
        Some(TicketReviewState::Scoped(_))
    ));
}

#[test]
fn an_issue_without_complete_baseline_keeps_an_explicit_hold_and_refuses_late_repair() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    assert_eq!(log.readiness(), RepositoryReviewReadiness::MissingBaseline);
    log.issue();
    assert_eq!(
        log.readiness(),
        RepositoryReviewReadiness::MissingAfterIssue
    );
    log.refuses(Record::TicketBaselineBatchRecorded(log.baseline()));
    log.assert_pair();
}

#[test]
fn declared_batches_refuse_missing_reordered_foreign_root_and_round_members() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let baseline = log.baseline();
    let mut missing = baseline.clone();
    missing.members.pop();
    log.refuses(Record::TicketBaselineBatchRecorded(missing));
    let mut reordered = baseline.clone();
    reordered.members.reverse();
    log.refuses(Record::TicketBaselineBatchRecorded(reordered));
    let mut foreign = baseline.clone();
    if let BaselineOutcome::Captured { inode, .. } = &mut foreign.members[0].outcome {
        *inode += 1;
    }
    log.refuses(Record::TicketBaselineBatchRecorded(foreign));
    let mut wrong_round = baseline.clone();
    wrong_round.binding.ticket.round += 1;
    log.refuses(Record::TicketBaselineBatchRecorded(wrong_round));
    let mut wrong_start = baseline.clone();
    wrong_start.binding.start_lsn = Lsn(wrong_start.binding.start_lsn.0 - 1);
    log.refuses(Record::TicketBaselineBatchRecorded(wrong_start));
    let baseline_lsn = log.append(Record::TicketBaselineBatchRecorded(baseline));
    let turn = log.issue();
    let check_lsn = log.check(Some(turn), true);
    let stamp = log.stamp(baseline_lsn, check_lsn);
    let mut missing = stamp.clone();
    missing.members.pop();
    log.refuses(Record::TicketReviewBatchStamped(missing));
    let mut foreign = stamp.clone();
    if let RepositoryStampOutcome::Sampled { root, .. } = &mut foreign.members[0].outcome {
        root.inode += 1;
    }
    log.refuses(Record::TicketReviewBatchStamped(foreign));
    let unbound_check = log.check(None, true);
    log.refuses(Record::TicketReviewBatchStamped(RepositoryStampBatch {
        check_lsn: unbound_check,
        ..stamp.clone()
    }));
    let wrong_check = log.append(Record::PredicateResult {
        goal: log.goal,
        turn: Some(turn),
        provenance: Provenance::Observed,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["/usr/bin/false".into()],
            program: Some("/usr/bin/false".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    log.refuses(Record::TicketReviewBatchStamped(stamp.clone()));
    log.refuses(Record::TicketReviewBatchStamped(RepositoryStampBatch {
        check_lsn: wrong_check,
        ..stamp
    }));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::MissingCheck);
}

#[test]
fn a_bad_batch_behind_a_warm_cursor_is_refused_before_replay_or_repair() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let mut malformed = log.baseline();
    malformed.members.reverse();
    let bad = write_record(
        &mut log.writer,
        Record::TicketBaselineBatchRecorded(malformed),
    );
    let tail = write_record(
        &mut log.writer,
        Record::Pause {
            agent: log.story.hire.id,
            browser: None,
        },
    );
    log.projection.apply(tail.0, tail.1, &tail.2).unwrap();
    assert!(log.projection.last_applied_lsn() > bad.0);
    let expected = log.projection.export().unwrap();
    drop(log.writer);
    let path = dudes_journal::segment::segment_path(&fixture.journal_dir(), bad.0.segment());
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        Reader::open(&fixture.journal_dir()),
        Err(dudes_journal::Error::Repository { .. })
    ));
    assert!(matches!(
        replay(&Reader::new(&fixture.journal_dir()), &mut log.projection),
        Err(dudes_journal::Error::Repository { .. })
    ));
    assert_eq!(log.projection.export().unwrap(), expected);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn an_already_satisfied_ticket_records_its_review_without_issuing_a_turn() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let early_check = log.check(None, true);
    assert!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .check
            .is_none()
    );
    let baseline = log.baseline();
    let baseline_lsn = log.append(Record::TicketBaselineBatchRecorded(baseline));
    log.refuses(Record::TicketReviewBatchStamped(
        log.stamp(baseline_lsn, early_check),
    ));
    let failed_check = log.check(None, false);
    let failed_stamp = log.stamp(baseline_lsn, failed_check);
    log.append(Record::TicketReviewBatchStamped(failed_stamp));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::CheckFailed);
    let check_lsn = log.check(None, true);
    let stamp = log.stamp(baseline_lsn, check_lsn);
    log.append(Record::TicketReviewBatchStamped(stamp));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::Complete);
    assert!(log.world.turns.is_empty());
    assert!(log.projection.turns().unwrap().is_empty());
    log.append(Record::Pause {
        agent: log.story.hire.id,
        browser: None,
    });
    drop(log);
    let host = SharedHost::new();
    let (mut running, settings) = start(&fixture, &host);
    let response = Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::AgentList,
            Some(json!({})),
            credential(&settings),
        )
        .unwrap();
    assert!(response.error.is_none());
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| record.turn_issue().is_some())
    );
}
