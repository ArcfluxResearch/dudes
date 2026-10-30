use super::*;
use duded::diff_capture::DiffCapture;
use duded::git_read::Tree;
use duded::git_read::diff::{BaselineCapture, DiffReader, ReviewSubject, RootIdentity};
use dudes_supervisor::Input;
use dudes_supervisor::testing::{Harness, Seen};

fn subjects(
    grant: &RepositoryTurnGrant,
    hire: &ScopedRepositories,
    baseline: Option<&RepositoryBaselineBatch>,
) -> Vec<ReviewSubject> {
    grant
        .repositories
        .entries()
        .iter()
        .map(|entry| ReviewSubject {
            repo_id: Some(entry.repo_id.clone()),
            tree: Tree::recorded(&entry.value, hire).unwrap(),
            baseline: baseline
                .and_then(|batch| {
                    batch
                        .members
                        .iter()
                        .find(|member| member.repo_id == entry.repo_id)
                })
                .and_then(|member| {
                    let BaselineOutcome::Captured {
                        device,
                        inode,
                        head,
                        preexisting_tracked,
                        preexisting_untracked,
                    } = member.outcome
                    else {
                        return None;
                    };
                    Some(BaselineCapture {
                        root: RootIdentity { device, inode },
                        head,
                        preexisting_tracked,
                        preexisting_untracked,
                    })
                }),
            stamp: None,
        })
        .collect()
}

fn sync(log: &mut ReviewLog, harness: &mut Harness, effects: Vec<Seen>) {
    let mut pending = std::collections::VecDeque::from(effects);
    let mut steps = 0;
    while let Some(effect) = pending.pop_front() {
        steps += 1;
        assert!(
            steps < 64,
            "the bounded fixture settles its emitted effects"
        );
        let next = if let Seen::Persist { token, entry } = effect {
            let (lsn, crc) = log.writer.append_witnessed(&entry).unwrap();
            apply_pair(
                &mut log.world,
                &mut log.projection,
                &(lsn, crc, entry.clone()),
            );
            log.assert_pair();
            Some(Input::Persisted {
                token,
                append: dudes_supervisor::input::RecordedAppend::try_new(lsn, (lsn, entry), None)
                    .unwrap(),
            })
        } else {
            harness.follow_up(effect)
        };
        if let Some(next) = next {
            pending.extend(harness.step(next));
        }
    }
    assert_eq!(
        log.world.goals[&log.goal].review,
        harness.supervisor.world().goals[&log.goal].review
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scratch journey keeps capture, issue, check and durable review positions together"
)]
fn real_three_repository_capture_and_checks_keep_the_declared_pair_and_fake_process_held() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let agent = log.story.hire.id;
    let full_hire = log.story.hire.repositories.clone();
    let mut harness = Harness::new();
    harness.launch = false;
    harness.answer_captures = false;
    for entry in Reader::new(&fixture.journal_dir())
        .iter_from(Lsn::ZERO)
        .unwrap()
    {
        let (lsn, entry) = entry.unwrap();
        harness
            .supervisor
            .handle(
                Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    harness
        .supervisor
        .observe_repository_admission(dudes_supervisor::RepositoryAdmission::new([(
            agent,
            log.story.hired_lsn,
            true,
        )]));
    let effects = harness.step(Input::Spend {
        agent,
        view: dudes_supervisor::SpendView::default(),
    });
    sync(&mut log, &mut harness, effects);
    let effects = harness.step(Input::Started);
    sync(&mut log, &mut harness, effects);
    assert!(
        !harness
            .records()
            .any(|record| record.turn_issue().is_some())
    );
    assert!(
        harness.host.spawned.is_empty(),
        "the process seam is fake and stays closed"
    );
    let capture = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::CaptureRepositoryBaseline { capture } => Some(capture.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        capture
            .grant
            .repositories
            .entries()
            .iter()
            .map(|entry| entry.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api"]
    );
    let reader = Arc::new(DiffReader::new(
        Arc::new(RealRunner(OneShot::new())),
        git_path(),
        fixture.runtime.path(),
    ));
    let service = DiffCapture::new(reader, Box::new(|| Timestamp::from_unix_millis(1_000)));
    let baseline = service.baseline_batch(
        &subjects(&capture.grant, &full_hire, None),
        capture.binding,
        Arc::default(),
    );
    assert!(baseline.complete(), "{baseline:?}");
    let effects = harness.step(Input::RepositoryBaselineCaptured {
        capture: capture.clone(),
        baseline: baseline.clone(),
    });
    sync(&mut log, &mut harness, effects);
    let review = log.world.repository_review(log.goal).unwrap();
    assert!(review.baseline.as_ref().unwrap().0 < review.first_issue_lsn.unwrap());
    let check_root = log.world.predicate_directory(log.goal).unwrap();
    let api = &capture
        .grant
        .repositories
        .get(&RepoId::parse("api").unwrap())
        .unwrap()
        .execution
        .directory;
    assert_eq!(check_root, api);
    assert_ne!(
        check_root,
        capture.grant.repositories.primary().execution.directory
    );
    let roots = log.world.agents[&agent].repositories.check_program_roots();
    let runner = RealRunner(OneShot::new());
    let (_, cwd_check) = duded::predicate::spec_for_repositories(
        &["/bin/pwd".into()],
        check_root,
        &[],
        git_path().as_deref(),
        &roots,
    )
    .unwrap();
    let PredicateOutcome::Finished {
        exit: Some(0),
        output_tail,
        ..
    } = runner.run(&cwd_check, PROCESS_BOUND).unwrap()
    else {
        panic!("the real selected-directory check ran")
    };
    assert_eq!(Path::new(output_tail.trim()), check_root);
    let queries = capture
        .grant
        .repositories
        .entries()
        .iter()
        .map(|entry| entry.value.execution.directory.clone())
        .collect::<Vec<_>>();
    let changed = dudes_config::check::changed_check();
    let docs = &log
        .story
        .hire
        .repositories
        .repositories
        .get(&RepoId::parse("docs").unwrap())
        .unwrap()
        .execution
        .directory;
    std::fs::write(docs.join("tracked.txt"), "outside the declaration\n").unwrap();
    assert!(
        !duded::predicate::files_changed(
            &runner,
            &changed,
            &queries,
            &[],
            git_path().as_deref(),
            &roots,
            PROCESS_BOUND
        )
        .unwrap()
        .unwrap()
        .0
    );
    std::fs::write(queries[0].join("tracked.txt"), "web change\n").unwrap();
    assert!(
        duded::predicate::files_changed(
            &runner,
            &changed,
            &queries,
            &[],
            git_path().as_deref(),
            &roots,
            PROCESS_BOUND
        )
        .unwrap()
        .unwrap()
        .0
    );
    std::fs::write(queries[1].join("tracked.txt"), "api change\n").unwrap();
    let dudes_proto::Predicate::Command { argv, .. } = &log.world.goals[&log.goal].goal.predicate
    else {
        unreachable!()
    };
    let (program, check) = duded::predicate::spec_for_repositories(
        argv,
        check_root,
        &[],
        git_path().as_deref(),
        &roots,
    )
    .unwrap();
    let outcome = runner.run(&check, PROCESS_BOUND).unwrap();
    let (pass, evidence) = duded::predicate::judge(&outcome, argv, Some(program)).unwrap();
    let effects = harness.step(Input::PredicateDone {
        goal: log.goal,
        pass,
        evidence,
    });
    sync(&mut log, &mut harness, effects);
    let (baseline_lsn, check_lsn) = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::CaptureRepositoryStamp {
                baseline_lsn,
                check_lsn,
                ..
            } => Some((*baseline_lsn, *check_lsn)),
            _ => None,
        })
        .unwrap();
    let stamp = service.stamp_batch(
        &subjects(&capture.grant, &full_hire, Some(&baseline)),
        capture.binding,
        baseline_lsn,
        check_lsn,
        Arc::default(),
    );
    assert!(stamp.complete());
    let effects = harness.step(Input::RepositoryStampCaptured {
        capture: capture.clone(),
        stamp: stamp.clone(),
    });
    sync(&mut log, &mut harness, effects);
    assert_eq!(log.readiness(), RepositoryReviewReadiness::Complete);
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&stamp),
        DiffFreshness::MatchesReviewSample
    );
    std::fs::rename(&queries[1], queries[1].with_extension("missing")).unwrap();
    assert!(
        duded::predicate::files_changed(
            &runner,
            &changed,
            &queries,
            &[],
            git_path().as_deref(),
            &roots,
            PROCESS_BOUND
        )
        .unwrap_or(None)
        .is_none()
    );
    let unknown = service.stamp_batch(
        &subjects(&capture.grant, &full_hire, Some(&baseline)),
        capture.binding,
        baseline_lsn,
        check_lsn,
        Arc::default(),
    );
    assert!(!unknown.complete());
    assert_eq!(
        log.world
            .repository_review(log.goal)
            .unwrap()
            .freshness(&unknown),
        DiffFreshness::Unknown
    );
    log.append(Record::TicketReviewBatchStamped(unknown));
    assert_eq!(log.readiness(), RepositoryReviewReadiness::UnknownStamp);
    assert!(harness.host.spawned.is_empty());
}
