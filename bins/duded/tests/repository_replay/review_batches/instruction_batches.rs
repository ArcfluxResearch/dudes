use super::*;
use dudes_proto::instruction::*;
use dudes_proto::record::{Actor, Hex32};
use dudes_proto::repository_instruction::*;
use dudes_types::{CardStatus, Mode};

fn hash(bytes: &[u8]) -> Hex32 {
    Hex32::new(*blake3::hash(bytes).as_bytes())
}

fn fingerprint(path: &Path) -> InstructionFingerprint {
    let bytes = std::fs::read(path).unwrap();
    InstructionFingerprint::File {
        digest: hash(&bytes),
        bytes: bytes.len() as u64,
    }
}

fn starting(
    log: &ReviewLog,
    turn: TurnId,
    issue: InstructionIssueAssociation,
) -> RepositoryInstructionStart {
    let agent = log.story.hire.id;
    let round = log.world.ticket_repository_round(log.goal);
    let ticket = round.and_then(|round| {
        let card = log.projection.card(round.card).unwrap().unwrap();
        log.world
            .instructions
            .association_for_goal(log.goal, card.seq)
    });
    RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding: RepositoryInstructionBinding {
            hired_lsn: log.story.hired_lsn,
            ticket_start_lsn: round.map(|round| round.start_lsn),
        },
        owner: InstructionObservationOwner::Managed { agent, turn, issue },
        ticket,
        members: log
            .story
            .hire
            .repositories
            .repositories
            .entries()
            .iter()
            .map(|member| {
                let copy = &member.value;
                let context = InstructionPathContext::OwnedWorktree {
                    lease: copy.worktree,
                };
                let root = NativeInstructionPath::from_path(&copy.execution.directory).unwrap();
                let path = copy.execution.directory.join("AGENTS.md");
                std::fs::write(&path, "initial instructions\n").unwrap();
                let discovery = InstructionDiscoveryPlan {
                    roots: vec![InstructionRoot {
                        path: root.clone(),
                        kind: InstructionRootKind::Project,
                        context,
                    }],
                    rules: vec![InstructionDiscoveryRule {
                        root: 0,
                        candidate: InstructionCandidate::Fixed {
                            paths: vec![
                                NativeInstructionPath::from_path(Path::new("AGENTS.md")).unwrap(),
                            ],
                        },
                        selection: InstructionSelection::All,
                        activation: InstructionActivation::Active,
                        imports: InstructionImportPolicy::None,
                        evidence: InstructionDiscoveryEvidence {
                            kind: InstructionDiscoveryEvidenceKind::DaemonComposition,
                            digest: hash(b"fixture-discovery"),
                        },
                    }],
                    configuration_digest: None,
                    gaps: Vec::new(),
                };
                RepositoryInstructionSnapshot {
                    repo_id: member.repo_id.clone(),
                    root: Some(copy.execution.directory_identity),
                    snapshot: InstructionSnapshot {
                        sampled_at: Timestamp::UNIX_EPOCH,
                        source: InstructionSnapshotSource::Fresh,
                        boot: None,
                        pid_namespace: None,
                        execution_folder: Some(root.clone()),
                        participants: vec![InstructionParticipant {
                            agent,
                            provider: dudes_types::Provider::Claude,
                            mode: InstructionDeclarationMode::ClaudeDefault,
                            declaration_digest: hash(b"fixture-discovery"),
                            discovery,
                        }],
                        inputs: vec![InstructionInput {
                            path: NativeInstructionPath::from_path(&path).unwrap(),
                            resolved_path: Some(NativeInstructionPath::from_path(&path).unwrap()),
                            context,
                            source: InstructionInputSource {
                                agent,
                                rule: 0,
                                root,
                                kind: InstructionSourceKind::Fixed,
                            },
                            activation: InstructionActivation::Active,
                            shadowed: false,
                            fingerprint: fingerprint(&path),
                        }],
                        inventoried: vec![InstructionRuleRef { agent, rule: 0 }],
                        gaps: Vec::new(),
                        usage: InstructionSampleUsage {
                            source_bytes: 21,
                            ..InstructionSampleUsage::default()
                        },
                    },
                }
            })
            .collect(),
    }
}

fn ended(log: &mut ReviewLog, turn: TurnId) -> Lsn {
    let mut record = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent,
        turn: actual,
        mode,
        provenance,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *agent = log.story.hire.id;
    *actual = turn;
    *mode = Mode::Managed;
    *provenance = Provenance::Observed;
    log.append(record)
}

fn finishing(
    start: &RepositoryInstructionStart,
    started: Lsn,
    end: Lsn,
) -> RepositoryInstructionFinish {
    let InstructionObservationOwner::Managed { turn, .. } = start.owner else {
        unreachable!()
    };
    RepositoryInstructionFinish {
        v: INSTRUCTION_BATCH_VERSION,
        binding: start.binding,
        owner: start.owner,
        ticket: start.ticket,
        baseline: InstructionBaselineRef::Started { lsn: started },
        members: start
            .members
            .iter()
            .map(|member| {
                let mut comparison = InstructionComparison::default();
                for input in &member.snapshot.inputs {
                    let after = fingerprint(&input.path.to_path().unwrap());
                    if let InstructionFingerprint::File { bytes, .. } = after {
                        comparison.usage.source_bytes += bytes;
                    }
                    if after != input.fingerprint {
                        comparison.changes.push(InstructionPathChange {
                            path: input.path.clone(),
                            context: input.context,
                            source: input.source.clone(),
                            before: input.fingerprint,
                            after,
                        });
                    }
                }
                RepositoryInstructionSettlement {
                    repo_id: member.repo_id.clone(),
                    ending_root: member.root,
                    settlement: InstructionSettlement::Compared {
                        end: InstructionEndBoundary::ManagedTurn { turn, end_lsn: end },
                        sampled_at: Timestamp::UNIX_EPOCH,
                        recovery: false,
                        comparison,
                    },
                }
            })
            .collect(),
    }
}

fn refuses(log: &mut ReviewLog, record: Record) {
    let before = log.projection.export().unwrap();
    let state = log.world.instructions.clone();
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    };
    let lsn = Lsn(log.projection.last_applied_lsn().0 + 1);
    assert!(log.world.apply_checked(lsn, &entry).is_err());
    assert!(log.projection.preflight_instruction(lsn, &entry).is_err());
    assert!(log.projection.apply(lsn, 1, &entry).is_err());
    assert_eq!(log.world.instructions, state);
    assert_eq!(log.projection.export().unwrap(), before);
}

fn accept(log: &ReviewLog, watermark: InstructionReviewWatermark) -> Record {
    let card = log.world.ticket_repository_round(log.goal).unwrap().card;
    Record::TicketInstructionReviewed {
        card,
        by: Actor::Peer {
            pid: 77,
            uid: Some(501),
        },
        expected_seq: log.projection.card(card).unwrap().unwrap().seq,
        watermark,
        action: InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Continue,
        },
    }
}

#[test]
fn an_owned_undeclared_instruction_change_keeps_its_repo_and_survives_warm_and_cold_replay() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let turn = TurnId::from_parts(60, 1);
    let start = starting(&log, turn, InstructionIssueAssociation::Prospective);
    let card = start.ticket.unwrap().card;
    assert_eq!(
        start
            .members
            .iter()
            .map(|m| m.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    let started = log.append(Record::InstructionObservationBatchStarted(start.clone()));
    let old_watermark = log
        .world
        .instructions
        .watermark(card, hash)
        .unwrap()
        .unwrap();
    assert_eq!(log.issue(), turn);
    assert_eq!(
        log.world.turns[&turn]
            .repositories
            .as_ref()
            .unwrap()
            .grant()
            .unwrap()
            .repositories
            .entries()
            .len(),
        2
    );
    let instruction = start.members[2].snapshot.inputs[0].path.to_path().unwrap();
    std::fs::write(&instruction, "revised instructions\n").unwrap();
    let end = ended(&mut log, turn);
    let finish = finishing(&start, started, end);
    log.append(Record::InstructionObservationBatchFinished(finish));
    let review = log.world.instructions.review(card, hash).unwrap().unwrap();
    assert!(!review.coverage_unknown);
    assert!(!review.pending_comparison);
    assert_eq!(review.changed_paths.len(), 1);
    assert_eq!(
        review.changed_paths[0].repo_id.as_ref().unwrap().as_str(),
        "docs"
    );
    assert_eq!(review.changed_paths[0].path.to_path().unwrap(), instruction);
    assert!(matches!(
        review.changed_paths[0].context,
        InstructionPathContext::OwnedWorktree { .. }
    ));
    assert_ne!(review.watermark, old_watermark);
    assert!(log.world.instructions.pending_for_goal(log.goal));
    assert_eq!(
        log.world.instructions,
        log.projection.instruction_state().unwrap()
    );
    let stale_request = accept(&log, old_watermark);
    refuses(&mut log, stale_request);
    let expected = log.projection.export().unwrap();
    let state = log.world.instructions.clone();
    drop(log);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    let mut cold = Projections::in_memory().unwrap();
    replay(&reader, &mut cold).unwrap();
    assert_eq!(warm.export().unwrap(), expected);
    assert_eq!(cold.export().unwrap(), expected);
    assert_eq!(warm.instruction_state().unwrap(), state);
    assert_eq!(cold.instruction_state().unwrap(), state);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one real repository fixture covers malformed evidence and the socket refusal without repeating setup"
)]
fn malformed_members_and_explicit_unknown_cannot_qualify_accept_or_an_ordinary_done_move() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let turn = TurnId::from_parts(60, 1);
    let start = starting(&log, turn, InstructionIssueAssociation::Prospective);
    for malformed in [
        {
            let mut b = start.clone();
            b.members.pop();
            b
        },
        {
            let mut b = start.clone();
            b.members.reverse();
            b
        },
        {
            let mut b = start.clone();
            b.members[0].root.as_mut().unwrap().inode += 1;
            b
        },
        {
            let mut b = start.clone();
            b.binding.hired_lsn = Lsn::ZERO;
            b
        },
    ] {
        refuses(
            &mut log,
            Record::InstructionObservationBatchStarted(malformed),
        );
    }
    let started = log.append(Record::InstructionObservationBatchStarted(start.clone()));
    log.issue();
    let end = ended(&mut log, turn);
    std::fs::write(
        start.members[2].snapshot.inputs[0].path.to_path().unwrap(),
        "known change before the cap\n",
    )
    .unwrap();
    let mut finish = finishing(&start, started, end);
    let mut missing = finish.clone();
    missing.members.pop();
    refuses(
        &mut log,
        Record::InstructionObservationBatchFinished(missing),
    );
    let mut reordered = finish.clone();
    reordered.members.reverse();
    refuses(
        &mut log,
        Record::InstructionObservationBatchFinished(reordered),
    );
    let mut wrong_attempt = finish.clone();
    wrong_attempt.baseline = InstructionBaselineRef::Started { lsn: end };
    refuses(
        &mut log,
        Record::InstructionObservationBatchFinished(wrong_attempt),
    );
    let gap = InstructionCoverageGap {
        reason: InstructionGapKind::MetadataLimit,
        context: start.members[2].snapshot.inputs[0].context,
        agent: Some(log.story.hire.id),
        path: Some(start.members[2].snapshot.inputs[0].path.clone()),
        origin: InstructionGapOrigin::Observed,
    };
    if let InstructionSettlement::Compared { comparison, .. } = &mut finish.members[2].settlement {
        comparison.gaps.push(gap);
    }
    log.append(Record::InstructionObservationBatchFinished(finish));
    let card = start.ticket.unwrap().card;
    let review = log.world.instructions.review(card, hash).unwrap().unwrap();
    assert!(review.coverage_unknown);
    assert_eq!(
        review.changed_paths[0].repo_id.as_ref().unwrap().as_str(),
        "docs"
    );
    assert_eq!(review.repository_gaps[0].repo_id.as_str(), "docs");
    let accepting = accept(&log, review.watermark);
    refuses(&mut log, accepting);
    let expected_seq = log.projection.card(card).unwrap().unwrap().seq;
    refuses(
        &mut log,
        Record::CardTransition {
            card,
            from: CardStatus::Blocked,
            to: CardStatus::Done,
            by: Actor::System,
            expected_seq,
            reason: None,
        },
    );
    log.append(Record::Pause {
        agent: log.story.hire.id,
        browser: None,
    });
    drop(log);
    let host = SharedHost::new();
    let (mut running, settings) = super::super::start(&fixture, &host);
    let mut socket = Client::connect(settings.socket()).unwrap();
    let detail = socket
        .call_as_operator(
            dudes_proto::rpc::Method::TicketRead,
            Some(json!({"card":card})),
            credential(&settings),
        )
        .unwrap();
    let detail: dudes_proto::rpc::ticket::TicketReadResult =
        serde_json::from_value(detail.result.unwrap()).unwrap();
    assert!(
        detail
            .ticket
            .instruction_review
            .as_ref()
            .unwrap()
            .coverage_unknown
    );
    let before = fixture.records();
    let response = socket.call_as_operator(dudes_proto::rpc::Method::TicketAccept,
        Some(json!({"card":card,"expected_seq":detail.card.seq,"review_watermark":detail.ticket.instruction_review.unwrap().watermark})), credential(&settings)).unwrap();
    assert_eq!(
        response.error.unwrap().data.unwrap()["reason"],
        "instruction_coverage_unknown"
    );
    assert_eq!(fixture.records(), before);
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
}

#[test]
fn an_issued_unspawned_turn_can_still_record_its_first_physical_attempt() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let unissued = starting(
        &log,
        TurnId::from_parts(60, 1),
        InstructionIssueAssociation::Prospective,
    );
    let unissued_lsn = log.append(Record::InstructionObservationBatchStarted(unissued.clone()));
    log.append(Record::InstructionObservationBatchFinished(
        RepositoryInstructionFinish {
            v: INSTRUCTION_BATCH_VERSION,
            binding: unissued.binding,
            owner: unissued.owner,
            ticket: unissued.ticket,
            baseline: InstructionBaselineRef::Started { lsn: unissued_lsn },
            members: unissued
                .members
                .iter()
                .map(|member| RepositoryInstructionSettlement {
                    repo_id: member.repo_id.clone(),
                    ending_root: None,
                    settlement: InstructionSettlement::NoIssuedTurn,
                })
                .collect(),
        },
    ));
    assert!(!log.world.instructions.pending_for_goal(log.goal));
    let turn = log.issue();
    assert!(!log.world.instructions.repository_turn_covered(turn));
    let issue_lsn = log.world.turns[&turn].issue_lsn;
    let start = starting(
        &log,
        turn,
        InstructionIssueAssociation::Issued { issue_lsn },
    );
    let started = log.append(Record::InstructionObservationBatchStarted(start.clone()));
    assert_eq!(
        log.world.instructions.current_for_turn(turn).unwrap().0,
        started
    );
    let uncertain = RepositoryInstructionFinish {
        v: INSTRUCTION_BATCH_VERSION,
        binding: start.binding,
        owner: start.owner,
        ticket: start.ticket,
        baseline: InstructionBaselineRef::Started { lsn: started },
        members: start
            .members
            .iter()
            .map(|member| RepositoryInstructionSettlement {
                repo_id: member.repo_id.clone(),
                ending_root: None,
                settlement: InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                    end: None,
                    sampled_at: None,
                    recovery: true,
                    comparison: InstructionComparison {
                        changes: Vec::new(),
                        usage: InstructionSampleUsage::default(),
                        gaps: vec![InstructionCoverageGap {
                            reason: InstructionGapKind::UnrecordedLaunchBoundary,
                            context: InstructionPathContext::Unresolved,
                            agent: Some(log.story.hire.id),
                            path: None,
                            origin: InstructionGapOrigin::Observed,
                        }],
                    },
                },
            })
            .collect(),
    };
    log.append(Record::InstructionObservationBatchFinished(uncertain));
    let retry = starting(
        &log,
        turn,
        InstructionIssueAssociation::Issued { issue_lsn },
    );
    let retry_lsn = log.append(Record::InstructionObservationBatchStarted(retry.clone()));
    assert_eq!(
        log.world.instructions.current_for_turn(turn).unwrap().0,
        retry_lsn
    );
    let end = ended(&mut log, turn);
    log.append(Record::InstructionObservationBatchFinished(finishing(
        &retry, retry_lsn, end,
    )));
    assert!(log.world.instructions.repository_turn_covered(turn));
    assert_eq!(log.world.instructions.attempts().count(), 3);
    assert!(log.world.instructions.pending_for_goal(log.goal));
    assert_eq!(
        log.world.instructions,
        log.projection.instruction_state().unwrap()
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one prefix proves pre-mutation refusal and rejects malformed history behind a warm cursor"
)]
fn a_prospective_unticketed_observation_cannot_be_reused_by_a_new_ticket_issue() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let ticket = ticket_start(&story);
    let (writer, world, projection) = replay_pair(&fixture);
    let mut log = ReviewLog {
        writer,
        world,
        projection,
        story,
        goal: ticket.goal.id,
    };
    let turn = TurnId::from_parts(60, 1);
    let unticketed = starting(&log, turn, InstructionIssueAssociation::Prospective);
    assert!(unticketed.ticket.is_none());
    let recorded = write_record(
        &mut log.writer,
        Record::InstructionObservationBatchStarted(unticketed),
    );
    apply_pair(&mut log.world, &mut log.projection, &recorded);
    let started = recorded.0;
    seed_ticket(&mut log.writer, &ticket);
    write_record(
        &mut log.writer,
        Record::TicketStartedWithRepositories(Box::new(ticket)),
    );
    for item in Reader::new(&fixture.journal_dir())
        .iter_from(Lsn::ZERO)
        .unwrap()
    {
        let (lsn, entry) = item.unwrap();
        if log.world.last_applied_lsn.is_none_or(|last| lsn > last) {
            log.world.apply_checked(lsn, &entry).unwrap();
        }
    }
    replay(&Reader::new(&fixture.journal_dir()), &mut log.projection).unwrap();
    log.assert_pair();
    let decision = log.append(Record::LoopDecided {
        agent: log.story.hire.id,
        input_digest: "new-ticket".into(),
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
    let payload = Record::TurnIssued {
        agent: log.story.hire.id,
        turn,
        goal: log.goal,
        task: "Repair both repositories".into(),
        session: None,
        mode: Mode::Managed,
        model_selection: None,
        lsn_of_decision: decision,
        answers: Vec::new(),
    };
    let issue = Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
        grant: RepositoryTurnGrant::select(
            log.story.hired_lsn,
            &log.story.hire.repositories,
            log.world.ticket_repository_round(log.goal),
        )
        .unwrap(),
        payload: payload.issue_payload().unwrap().to_owned(),
    }));
    let before = log.projection.export().unwrap();
    let prior = log.world.instructions.clone();
    let recorded = write_record(&mut log.writer, issue);
    assert!(log.world.apply_checked(recorded.0, &recorded.2).is_err());
    assert!(
        log.projection
            .preflight_instruction(recorded.0, &recorded.2)
            .is_err()
    );
    assert!(
        log.projection
            .apply(recorded.0, recorded.1, &recorded.2)
            .is_err()
    );
    assert_eq!(log.world.instructions, prior);
    assert_eq!(log.projection.export().unwrap(), before);
    assert!(
        log.world
            .instructions
            .attempt(started)
            .unwrap()
            .ticket
            .is_none()
    );
    let tail = write_record(
        &mut log.writer,
        Record::Pause {
            agent: log.story.hire.id,
            browser: None,
        },
    );
    log.projection.apply(tail.0, tail.1, &tail.2).unwrap();
    let warm = log.projection.export().unwrap();
    assert!(Reader::open(&fixture.journal_dir()).is_err());
    assert!(replay(&Reader::new(&fixture.journal_dir()), &mut log.projection).is_err());
    assert_eq!(log.projection.export().unwrap(), warm);
}

#[test]
fn foreign_source_and_ending_roots_cannot_qualify_a_complete_comparison() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let turn = TurnId::from_parts(60, 1);
    let observation = starting(&log, turn, InstructionIssueAssociation::Prospective);
    let started = log.append(Record::InstructionObservationBatchStarted(
        observation.clone(),
    ));
    log.issue();
    std::fs::write(
        observation.members[2].snapshot.inputs[0]
            .path
            .to_path()
            .unwrap(),
        "known revised instruction\n",
    )
    .unwrap();
    let end = ended(&mut log, turn);
    let finish = finishing(&observation, started, end);
    let mut foreign_source = finish.clone();
    if let InstructionSettlement::Compared { comparison, .. } =
        &mut foreign_source.members[2].settlement
    {
        comparison.changes[0].source.root = observation.members[0]
            .snapshot
            .execution_folder
            .clone()
            .unwrap();
    }
    let mut foreign_root = finish.clone();
    foreign_root.members[2].ending_root.as_mut().unwrap().inode += 1;
    let mut missing_root = finish;
    missing_root.members[2].ending_root = None;
    let candidates = [
        ("source", foreign_source),
        ("ending_root", foreign_root.clone()),
        ("missing_root", missing_root),
    ];
    let next = Lsn(log.projection.last_applied_lsn().0 + 1);
    let accepted: Vec<_> = candidates
        .iter()
        .filter_map(|(name, batch)| {
            let entry = Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::InstructionObservationBatchFinished(batch.clone()),
            };
            log.projection
                .preflight_instruction(next, &entry)
                .is_ok()
                .then_some(*name)
        })
        .collect();
    assert!(
        accepted.is_empty(),
        "complete comparisons accepted invalid provenance: {accepted:?}"
    );
    for (_, batch) in candidates {
        refuses(&mut log, Record::InstructionObservationBatchFinished(batch));
    }
    if let InstructionSettlement::Compared { comparison, .. } =
        &mut foreign_root.members[2].settlement
    {
        comparison.gaps.push(InstructionCoverageGap {
            reason: InstructionGapKind::PathUnresolved,
            context: InstructionPathContext::Unresolved,
            agent: Some(log.story.hire.id),
            path: observation.members[2].snapshot.execution_folder.clone(),
            origin: InstructionGapOrigin::Observed,
        });
    }
    log.append(Record::InstructionObservationBatchFinished(foreign_root));
    let card = observation.ticket.unwrap().card;
    let review = log.world.instructions.review(card, hash).unwrap().unwrap();
    assert!(review.coverage_unknown);
    assert_eq!(
        review.changed_paths[0].repo_id.as_ref().unwrap().as_str(),
        "docs"
    );
    assert_eq!(
        log.world.instructions,
        log.projection.instruction_state().unwrap()
    );
}

fn add_external_input(
    snapshot: &mut InstructionSnapshot,
    rule: u16,
    path: &Path,
    kind: InstructionSourceKind,
    context: InstructionPathContext,
) {
    let participant = &snapshot.participants[0];
    let root = participant.discovery.roots
        [usize::from(participant.discovery.rules[usize::from(rule)].root)]
    .path
    .clone();
    let observed = fingerprint(path);
    if let InstructionFingerprint::File { bytes, .. } = observed {
        snapshot.usage.source_bytes += bytes;
    }
    snapshot.inputs.push(InstructionInput {
        path: NativeInstructionPath::from_path(path).unwrap(),
        resolved_path: Some(NativeInstructionPath::from_path(path).unwrap()),
        context,
        source: InstructionInputSource {
            agent: participant.agent,
            rule,
            root,
            kind,
        },
        activation: InstructionActivation::Active,
        shadowed: false,
        fingerprint: observed,
    });
}

#[test]
fn frozen_ancestor_profile_and_import_sources_remain_reviewable_outside_the_checkout() {
    let fixture = Fixture::new();
    let mut log = ReviewLog::new(&fixture);
    let turn = TurnId::from_parts(60, 1);
    let mut observation = starting(&log, turn, InstructionIssueAssociation::Prospective);
    let copy = log.story.hire.repositories.repositories.primary();
    let ancestor = copy
        .execution
        .directory
        .parent()
        .unwrap()
        .join("ANCESTOR.md");
    let profile = fixture.runtime.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let profile_file = profile.join("AGENTS.md");
    let imported = profile.join("imported.md");
    for path in [&ancestor, &profile_file, &imported] {
        std::fs::write(path, "initial external\n").unwrap();
    }
    for member in &mut observation.members {
        let sample = &mut member.snapshot;
        let participant = &mut sample.participants[0];
        let mut ancestor_rule = participant.discovery.rules[0].clone();
        ancestor_rule.candidate = InstructionCandidate::Ancestor {
            paths: vec![NativeInstructionPath::from_path(Path::new("ANCESTOR.md")).unwrap()],
            stop: InstructionAncestorStop::FileSystemRoot,
        };
        participant.discovery.rules.push(ancestor_rule);
        let profile_context = InstructionPathContext::SharedInput {
            source: InstructionSharedInput::Profile,
        };
        participant.discovery.roots.push(InstructionRoot {
            path: NativeInstructionPath::from_path(&profile).unwrap(),
            kind: InstructionRootKind::Profile,
            context: profile_context,
        });
        let mut profile_rule = participant.discovery.rules[0].clone();
        profile_rule.root = 1;
        profile_rule.imports = InstructionImportPolicy::ClaudeFourHops;
        participant.discovery.rules.push(profile_rule);
        add_external_input(
            sample,
            1,
            &ancestor,
            InstructionSourceKind::Ancestor,
            InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Ancestor,
            },
        );
        add_external_input(
            sample,
            2,
            &profile_file,
            InstructionSourceKind::Fixed,
            profile_context,
        );
        add_external_input(
            sample,
            2,
            &imported,
            InstructionSourceKind::Import {
                from: NativeInstructionPath::from_path(&profile_file).unwrap(),
                depth: 1,
            },
            InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Import,
            },
        );
    }
    let started = log.append(Record::InstructionObservationBatchStarted(
        observation.clone(),
    ));
    log.issue();
    for path in [&ancestor, &profile_file, &imported] {
        std::fs::write(path, "updated external\n").unwrap();
    }
    let end = ended(&mut log, turn);
    log.append(Record::InstructionObservationBatchFinished(finishing(
        &observation,
        started,
        end,
    )));
    let card = observation.ticket.unwrap().card;
    let review = log.world.instructions.review(card, hash).unwrap().unwrap();
    assert!(!review.coverage_unknown);
    assert_eq!(review.changed_paths.len(), 9);
    let profile_path = NativeInstructionPath::from_path(&profile_file).unwrap();
    assert_eq!(
        review
            .changed_paths
            .iter()
            .filter(|path| path.path == profile_path)
            .map(|path| path.repo_id.as_ref().unwrap().as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    assert_eq!(
        log.world.instructions,
        log.projection.instruction_state().unwrap()
    );
}
