use super::*;
use dudes_proto::repository::{RepositoryTurnGrant, RepositoryTurnIssued};
use dudes_proto::{Actor, ConversationChargeScope, Gate, GateStep, LoopDecision};
use dudes_types::{CardStatus, GoalId, MailId, MailState, Seq, Tokens, TurnId, Usage};

struct PairedIssues {
    writer: Writer,
    world: dudes_supervisor::World,
    projection: Projections,
    old_world: dudes_supervisor::World,
    old_store: Projections,
    story: Story,
}

impl PairedIssues {
    fn new(fixture: &Fixture) -> (Self, dudes_proto::repository::RepositoryTicketStarted) {
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
        let mut old_world = dudes_supervisor::World::default();
        let mut old_store = Projections::in_memory().unwrap();
        for item in Reader::new(&fixture.journal_dir())
            .iter_from(Lsn::ZERO)
            .unwrap()
        {
            let (lsn, entry) = item.unwrap();
            let record = match entry.record {
                Record::AgentHiredWithRepositories(hire) => legacy_hire(
                    hire.id,
                    &hire.repositories.repositories.primary().execution.directory,
                ),
                Record::TicketCreatedWithRepositories(created) => Record::TicketCreated {
                    card: created.card,
                    title: created.title,
                    description: created.description,
                    department: created.department,
                    project: created.declaration.project,
                    done_when: created.done_when,
                    by: created.by,
                    request: created.request,
                },
                Record::TicketEditedWithRepositories(edited) => Record::TicketEdited {
                    card: edited.card,
                    title: edited.title,
                    description: edited.description,
                    done_when: edited.done_when,
                    by: edited.by,
                    expected_seq: edited.expected_seq,
                },
                Record::TicketStartedWithRepositories(start) => Record::TicketStarted {
                    card: start.card,
                    agent: start.agent,
                    goal: start.goal,
                    expected_seq: start.expected_seq,
                },
                Record::ProjectRepositoriesRecorded { .. }
                | Record::RepositoryTrustSet { .. }
                | Record::WorktreeLeased { .. } => continue,
                record => record,
            };
            let entry = Entry { record, ..entry };
            old_world.apply_checked(lsn, &entry).unwrap();
            old_store.apply(lsn, 1, &entry).unwrap();
        }
        (
            Self {
                writer,
                world,
                projection,
                old_world,
                old_store,
                story,
            },
            start,
        )
    }

    fn append(&mut self, record: Record) -> Lsn {
        let scoped = if let Some(payload) = record.issue_payload() {
            let grant = RepositoryTurnGrant::select(
                self.story.hired_lsn,
                &self.story.hire.repositories,
                self.world
                    .active_repository_round(self.story.hire.id)
                    .unwrap(),
            )
            .unwrap();
            Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
                grant,
                payload: payload.to_owned(),
            }))
        } else {
            record.clone()
        };
        let recorded = write_record(&mut self.writer, scoped);
        apply_pair(&mut self.world, &mut self.projection, &recorded);
        let old_entry = Entry {
            record,
            at: recorded.2.at,
        };
        self.old_world
            .apply_checked(recorded.0, &old_entry)
            .unwrap();
        self.old_store.apply(recorded.0, 1, &old_entry).unwrap();
        if let Some(issue) = old_entry.record.turn_issue() {
            let old = self.old_store.turn(issue.turn).unwrap().unwrap();
            let new = self.projection.turn(issue.turn).unwrap().unwrap();
            assert_eq!(
                (&new.session, &new.model_selection, new.goal, new.status),
                (&old.session, &old.model_selection, old.goal, old.status)
            );
            let new_world = &self.world.turns[&issue.turn];
            let old_world = &self.old_world.turns[&issue.turn];
            assert_eq!(
                (&new_world.task, &new_world.answers),
                (&old_world.task, &old_world.answers)
            );
            let frozen = new.repositories.as_ref().unwrap();
            assert_eq!(
                frozen
                    .grant()
                    .unwrap()
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| entry.repo_id.as_str())
                    .collect::<Vec<_>>(),
                ["web", "api"]
            );
            assert_eq!(
                frozen.grant().unwrap().repositories.primary_id().as_str(),
                "web"
            );
            assert_eq!(self.world.turns[&issue.turn].repositories, new.repositories);
            if let Some(round) = frozen.grant().unwrap().ticket.as_ref() {
                let review = self.world.repository_review(round.goal).unwrap();
                assert_eq!(review.last_issue, Some((recorded.0, issue.turn)));
                assert_eq!(
                    review.readiness(),
                    dudes_proto::repository_review::RepositoryReviewReadiness::MissingAfterIssue
                );
                assert_eq!(
                    self.projection.goal(round.goal).unwrap().unwrap().review,
                    self.world.goals[&round.goal].review
                );
            }
            assert_eq!(
                self.world.instructions.issue_lsn(issue.turn),
                Some(recorded.0)
            );
            if old_entry.record.mail_issue().is_some()
                || old_entry.record.ordinary_issue().is_some()
            {
                self.assert_duplicate_refused(recorded.0, &recorded.2);
            }
            assert_eq!(
                self.world.browser_issue(issue.turn).map(|issue| (
                    issue.note_lsn,
                    issue.goal,
                    issue.completion_lsn
                )),
                self.old_world.browser_issue(issue.turn).map(|issue| (
                    issue.note_lsn,
                    issue.goal,
                    issue.completion_lsn
                ))
            );
        }
        recorded.0
    }

    fn assert_duplicate_refused(&mut self, issue_lsn: Lsn, entry: &Entry) {
        let issue = entry.record.turn_issue().unwrap();
        let before = self.projection.export().unwrap();
        let mut duplicate = entry.clone();
        if let Record::TurnIssuedWithRepositories(issue) = &mut duplicate.record
            && let dudes_proto::issue::ManagedIssuePayload::Ordinary { task, .. } =
                &mut issue.payload
        {
            task.push_str(" changed after issue");
        }
        assert!(matches!(
            self.projection.apply(Lsn(issue_lsn.0 + 1), 1, &duplicate),
            Err(dudes_journal::Error::Repository {
                reason: dudes_proto::repository_state::history::RepositoryStateError::Issue,
                ..
            })
        ));
        assert_eq!(self.projection.export().unwrap(), before);
        let mut invalid_world = self.world.clone();
        assert!(
            invalid_world
                .apply_checked(Lsn(issue_lsn.0 + 1), &duplicate)
                .is_err()
        );
        assert_eq!(
            invalid_world.turns[&issue.turn].task,
            self.world.turns[&issue.turn].task
        );
        assert_eq!(invalid_world.turns[&issue.turn].issue_lsn, issue_lsn);
        assert_eq!(
            invalid_world.turns[&issue.turn].repositories,
            self.world.turns[&issue.turn].repositories
        );
        assert_eq!(
            invalid_world.agents[&issue.agent].open_turn,
            self.world.agents[&issue.agent].open_turn
        );
        if let Some(goal) = issue.goal {
            assert_eq!(
                invalid_world.goals[&goal].turns_on_goal,
                self.world.goals[&goal].turns_on_goal
            );
        }
    }

    fn finish(&mut self, turn: TurnId) {
        let frozen = self.world.turns[&turn].clone();
        let mut ended = samples::record(RecordKind::TurnEnded);
        let Record::TurnEnded {
            agent,
            turn: actual,
            usage,
            model_usage,
            session,
            session_usage,
            outcome,
            ..
        } = &mut ended
        else {
            unreachable!()
        };
        *agent = self.story.hire.id;
        *actual = turn;
        *usage = Some(Usage {
            input: Tokens::new(7),
            usd: Some(dudes_types::Usd::from_micros(11)),
            ..Usage::ZERO
        });
        model_usage.clear();
        *session = None;
        *session_usage = None;
        *outcome = Some(dudes_proto::TurnOutcome::Completed);
        self.append(ended);
        let retained = self.projection.turn(turn).unwrap().unwrap();
        assert_eq!(retained.lsn_issued, frozen.issue_lsn);
        assert_eq!(retained.repositories, frozen.repositories);
        assert_eq!(self.world.turns[&turn].issue_lsn, frozen.issue_lsn);
        assert_eq!(self.world.turns[&turn].task, frozen.task);
    }

    fn issue(
        &mut self,
        kind: RecordKind,
        goal: GoalId,
        turn: TurnId,
        task: &str,
        fields: serde_json::Value,
    ) {
        let mut wire = serde_json::to_value(samples::record(kind)).unwrap();
        for (key, value) in
            json!({"agent": self.story.hire.id, "goal": goal, "turn": turn, "task": task,
            "session": null, "model_selection": {}, "answers": []})
            .as_object()
            .unwrap()
        {
            wire[key] = value.clone();
        }
        let serde_json::Value::Object(fields) = fields else {
            panic!("issue fields")
        };
        for (key, value) in fields {
            wire[key] = value;
        }
        let record = serde_json::from_value(wire).unwrap();
        self.append(record);
    }
}

fn trace() -> Vec<GateStep> {
    Gate::ORDER
        .into_iter()
        .map(|gate| GateStep {
            gate,
            fired: gate == Gate::Work,
            note: None,
        })
        .collect()
}

fn browser() -> dudes_proto::BrowserNoteAttribution {
    let Record::BrowserNoteQueued { browser, .. } = samples::record(RecordKind::BrowserNoteQueued)
    else {
        unreachable!()
    };
    browser
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one shared history compares nine issue families before warm and cold replay"
)]
fn nine_issue_families_replay_the_same_tasks_reservations_and_billing() {
    let fixture = Fixture::new();
    let (mut log, start) = PairedIssues::new(&fixture);
    let agent = start.agent;
    let goal = start.goal.id;
    let line = dudes_proto::task::IdentityLine::new(agent, "Kim", "engineer");
    for (number, kind) in [
        (1, RecordKind::TurnIssued),
        (2, RecordKind::TurnIssuedWithBrowserNote),
    ] {
        let task = "Repair both repositories";
        let note_lsn = (number == 2).then(|| {
            log.append(Record::BrowserNoteQueued {
                agent,
                text: task.into(),
                browser: browser(),
                destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal },
            })
        });
        let decision = log.append(Record::LoopDecided {
            agent,
            input_digest: "ordinary".into(),
            gate_trace: trace(),
            decision: LoopDecision::Continue { task: task.into() },
        });
        let turn = TurnId::from_parts(30, number);
        log.issue(
            kind,
            goal,
            turn,
            task,
            json!({"lsn_of_decision": decision, "note_lsn": note_lsn}),
        );
        log.finish(turn);
    }
    log.append(Record::PredicateResult {
        goal,
        turn: None,
        provenance: dudes_types::Provenance::Observed,
        pass: true,
        evidence: dudes_proto::Evidence::Command {
            argv: vec!["/usr/bin/true".into()],
            program: None,
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    let completion = log.append(Record::LoopDecided {
        agent,
        input_digest: "done".into(),
        gate_trace: trace(),
        decision: LoopDecision::Done,
    });
    for (number, kind, decision_kind) in [
        (3, RecordKind::NoteOnlyTurnIssued, "note_turn_decided"),
        (
            4,
            RecordKind::TaughtNoteOnlyTurnIssued,
            "taught_note_turn_decided",
        ),
        (
            5,
            RecordKind::IdentifiedTaughtNoteOnlyTurnIssued,
            "identified_taught_note_turn_decided",
        ),
    ] {
        let text = "Explain the result";
        let note = log.append(Record::BrowserNoteQueued {
            agent,
            text: text.into(),
            browser: browser(),
            destination: dudes_proto::BrowserNoteDestination::ManagedGoal { goal },
        });
        let task = match number {
            3 => text.to_owned(),
            4 => dudes_proto::taught_note_task(text).unwrap(),
            _ => dudes_proto::identified_taught_note_task(&line, text).unwrap(),
        };
        let decision = log.append(serde_json::from_value(json!({"t": decision_kind, "agent": agent, "goal": goal,
            "note_lsn": note, "completion_lsn": completion, "input_digest": "note", "gate_trace": trace(),
            "decision": {"kind": "issue", "task": task}})).unwrap());
        let turn = TurnId::from_parts(30, number);
        log.issue(
            kind,
            goal,
            turn,
            &task,
            json!({"lsn_of_decision": decision, "note_lsn": note, "completion_lsn": completion}),
        );
        log.finish(turn);
    }
    for (number, identified) in [(6, false), (7, true)] {
        let id = MailId::from_parts(40, number);
        let mut mail = samples::record(RecordKind::Mail);
        let Record::Mail {
            id: actual,
            to,
            state,
            ..
        } = &mut mail
        else {
            unreachable!()
        };
        *actual = id;
        *to = dudes_proto::MailTo::Agent { agent };
        *state = MailState::Durable;
        let receipt = dudes_proto::MailRef {
            id,
            lsn: log.append(mail),
        };
        let task = if identified {
            dudes_proto::MailWakeTask::identified(&line, 1)
        } else {
            dudes_proto::MailWakeTask::new(1)
        }
        .unwrap();
        let decision = |purpose, domain| {
            let decision = dudes_proto::MailWakeDecision {
                agent,
                goal,
                completion_lsn: completion,
                purpose,
                input_digest: dudes_proto::MailWakeDigest {
                    domain,
                    value: dudes_proto::record::Hex32::new([3; 32]),
                },
                gate_trace: trace(),
                decision: dudes_proto::MailWakeAdmission::Issue { task: task.clone() },
            };
            if identified {
                Record::IdentifiedMailWakeTurnDecided(decision)
            } else {
                Record::MailWakeTurnDecided(decision)
            }
        };
        let root = log.append(decision(
            dudes_proto::MailWakePurpose::FreshPreparation {
                plan: dudes_proto::MailWakePlan {
                    trigger: receipt,
                    frontier: receipt.lsn,
                    unread_count: 1,
                    task: task.clone(),
                    targets: vec![dudes_proto::MailWakeTarget {
                        mail: receipt,
                        original_count: 0,
                        target_count: 1,
                    }],
                },
            },
            dudes_proto::MailWakeDigestDomain::Fresh,
        ));
        log.append(Record::MailState {
            id,
            state: MailState::Notified { n: 1 },
        });
        let current = log.append(decision(
            dudes_proto::MailWakePurpose::ExistingPreparation { root_lsn: root },
            dudes_proto::MailWakeDigestDomain::Resume,
        ));
        let turn = TurnId::from_parts(30, number);
        log.issue(
            if identified {
                RecordKind::IdentifiedMailWakeOnlyTurnIssued
            } else {
                RecordKind::MailWakeOnlyTurnIssued
            },
            goal,
            turn,
            task.as_str(),
            json!({"root_lsn": root, "completion_lsn": completion, "lsn_of_decision": current}),
        );
        log.finish(turn);
        log.append(Record::MailState {
            id,
            state: MailState::Acked,
        });
    }
    let seq = log.projection.ticket(start.card).unwrap().unwrap().0.seq;
    log.append(Record::CardTransition {
        card: start.card,
        from: CardStatus::Doing,
        to: CardStatus::Review,
        by: Actor::System,
        expected_seq: seq,
        reason: None,
    });
    let context = log.append(Record::GoalCleared { agent, goal });
    let before = log
        .projection
        .spend_views(agent, goal, Timestamp::UNIX_EPOCH.utc_day())
        .unwrap();
    for (number, identified) in [(8, false), (9, true)] {
        let text = "Explain the next step";
        let note = log.append(Record::ConversationMessageQueued {
            agent,
            text: text.into(),
            browser: browser(),
            context_lsn: context,
            charge_scope: ConversationChargeScope::AgentAndFloor,
        });
        let task = if identified {
            dudes_proto::identified_conversation_task(&line, text, false)
        } else {
            text.into()
        };
        let decision = log.append(serde_json::from_value(json!({"t": if identified { "identified_conversation_turn_decided" } else { "conversation_turn_decided" },
            "agent": agent, "note_lsn": note, "context_lsn": context, "charge_scope": "agent_and_floor", "input_digest": "finite", "gate_trace": trace(),
            "decision": {"kind": "issue", "task": task}})).unwrap());
        let turn = TurnId::from_parts(30, number);
        log.issue(if identified { RecordKind::IdentifiedConversationTurnIssued } else { RecordKind::ConversationTurnIssued }, goal, turn, &task,
            json!({"note_lsn": note, "context_lsn": context, "charge_scope": "agent_and_floor", "lsn_of_decision": decision}));
        let row = &log.world.turns[&turn];
        assert_eq!(row.goal, None);
        let round = row.repositories.as_ref().unwrap().ticket().unwrap();
        assert_eq!((round.card, round.goal, round.round), (start.card, goal, 1));
        assert_eq!(
            log.world
                .instructions
                .association_for_goal(round.goal, Seq(0))
                .unwrap()
                .card,
            start.card
        );
        log.finish(turn);
    }
    let after = log
        .projection
        .spend_views(agent, goal, Timestamp::UNIX_EPOCH.utc_day())
        .unwrap();
    assert_eq!(after.goal, before.goal);
    assert_ne!(after.agent_today, before.agent_today);
    assert_eq!(log.projection.goal(goal).unwrap().unwrap().turns_on_goal, 7);
    assert_eq!(
        log.projection.chat().unwrap(),
        log.old_store.chat().unwrap()
    );
    assert_eq!(
        after,
        log.old_store
            .spend_views(agent, goal, Timestamp::UNIX_EPOCH.utc_day())
            .unwrap()
    );
    let mail_rows = |projection: &Projections| {
        projection
            .export()
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("mail_wake"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(mail_rows(&log.projection), mail_rows(&log.old_store));
    assert!(
        mail_rows(&log.projection)
            .iter()
            .any(|line| line.starts_with("mail_wake_by_day ") && line.ends_with(" 2"))
    );
    let ordinary = TurnId::from_parts(30, 1);
    let retained = log.projection.turn(ordinary).unwrap().unwrap();
    let saved_task = log.world.turns[&ordinary].task.clone();
    log.append(Record::AgentRetired {
        id: agent,
        reason: "finished".into(),
        browser_retire: None,
    });
    assert_eq!(
        log.projection.turn(ordinary).unwrap(),
        Some(retained.clone())
    );
    assert_eq!(log.world.turns[&ordinary].issue_lsn, retained.lsn_issued);
    assert_eq!(log.world.turns[&ordinary].task, saved_task);
    assert_eq!(
        log.world.turns[&ordinary].repositories,
        retained.repositories
    );
    let expected = log.projection.export().unwrap();
    drop(log);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut cold = Projections::in_memory().unwrap();
    replay(&reader, &mut cold).unwrap();
    assert_eq!(cold.export().unwrap(), expected);
}

#[test]
fn scoped_grants_refuse_changed_catalog_round_copy_and_trust_before_installing_a_turn() {
    let fixture = Fixture::new();
    let (mut log, start) = PairedIssues::new(&fixture);
    let grant = RepositoryTurnGrant::select(
        log.story.hired_lsn,
        &log.story.hire.repositories,
        log.world.active_repository_round(start.agent).unwrap(),
    )
    .unwrap();
    let mut payload = legacy_issue(start.agent)
        .issue_payload()
        .unwrap()
        .to_owned();
    let dudes_proto::issue::ManagedIssuePayload::Ordinary { goal, .. } = &mut payload else {
        unreachable!()
    };
    *goal = start.goal.id;
    let valid = RepositoryTurnIssued { grant, payload };
    let next = Lsn(log.projection.last_applied_lsn().0 + 1);
    let before = log.projection.export().unwrap();
    for change in 0..4 {
        let mut issue = valid.clone();
        match change {
            0 => issue.grant.catalog_lsn = Lsn(1),
            1 => issue.grant.hired_lsn = Lsn(1),
            2 => issue.grant.ticket.as_mut().unwrap().round += 1,
            _ => issue.grant.repositories = log.story.hire.repositories.repositories.clone(),
        }
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::TurnIssuedWithRepositories(Box::new(issue)),
        };
        assert!(log.projection.apply(next, 1, &entry).is_err(), "{change}");
        assert!(log.world.apply_checked(next, &entry).is_err(), "{change}");
        assert_eq!(log.projection.export().unwrap(), before);
        assert_eq!(log.world.agents[&start.agent].open_turn, None);
    }
    let repo = log.story.hire.repositories.repositories.entries()[2].clone();
    let mut lease_world = log.world.clone();
    let mut lease_store = Projections::in_memory().unwrap();
    replay(
        &Reader::open(&fixture.journal_dir()).unwrap().0,
        &mut lease_store,
    )
    .unwrap();
    let removed = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::WorktreeRemoved {
            worktree: repo.value.worktree,
            how: WorktreeRemoval::Operator,
        },
    };
    lease_store.apply(next, 1, &removed).unwrap();
    lease_world.apply_checked(next, &removed).unwrap();
    let issue = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TurnIssuedWithRepositories(Box::new(valid.clone())),
    };
    assert!(matches!(
        lease_store.apply(Lsn(next.0 + 1), 1, &issue),
        Err(dudes_journal::Error::Repository {
            reason: dudes_proto::repository_state::history::RepositoryStateError::Lease,
            ..
        })
    ));
    assert!(lease_world.apply_checked(Lsn(next.0 + 1), &issue).is_err());
    let revoked = write_record(
        &mut log.writer,
        Record::RepositoryTrustSet {
            project: "service".into(),
            repo_id: repo.repo_id,
            catalog_lsn: log.story.hire.repositories.catalog_lsn,
            origin: repo.value.origin,
            pin: None,
        },
    );
    apply_pair(&mut log.world, &mut log.projection, &revoked);
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TurnIssuedWithRepositories(Box::new(valid)),
    };
    let next = Lsn(log.projection.last_applied_lsn().0 + 1);
    assert!(matches!(
        log.projection.apply(next, 1, &entry),
        Err(dudes_journal::Error::Repository {
            reason: dudes_proto::repository_state::history::RepositoryStateError::Trust,
            ..
        })
    ));
    assert!(log.world.apply_checked(next, &entry).is_err());
}

#[test]
fn a_scoped_issue_replays_its_frozen_task_before_a_refused_provider_launch() {
    let fixture = Fixture::new();
    let (mut log, start) = PairedIssues::new(&fixture);
    let task = "Preserve this exact task through restart";
    let decision = log.append(Record::LoopDecided {
        agent: start.agent,
        input_digest: "ordinary".into(),
        gate_trace: trace(),
        decision: LoopDecision::Continue { task: task.into() },
    });
    let turn = TurnId::from_parts(30, 1);
    log.issue(
        RecordKind::TurnIssued,
        start.goal.id,
        turn,
        task,
        json!({"lsn_of_decision": decision}),
    );
    let saved = log.world.turns[&turn].clone();
    drop(log);
    let host = SharedHost::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let replayed = &daemon.world().turns[&turn];
    assert_eq!(replayed.task, saved.task);
    assert_eq!(replayed.repositories, saved.repositories);
    assert_eq!(replayed.answers, saved.answers);
    let mut running = common::start(daemon);
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
    let records = Reader::open(&fixture.journal_dir())
        .unwrap()
        .0
        .iter_from(Lsn::ZERO)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|(_, entry)| entry.record.turn_issue().is_some())
            .count(),
        1
    );
    assert!(
        !records
            .iter()
            .any(|(_, entry)| matches!(entry.record, Record::TurnSpawned { .. }))
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one joined instruction interval proves the held round survives goal clearing"
)]
fn pending_instruction_review_retains_scope_after_the_charge_goal_is_cleared() {
    use dudes_proto::repository_instruction::{
        INSTRUCTION_BATCH_VERSION, RepositoryInstructionBinding, RepositoryInstructionFinish,
        RepositoryInstructionSettlement, RepositoryInstructionSnapshot, RepositoryInstructionStart,
    };
    use dudes_proto::{
        InstructionBaselineRef, InstructionComparison, InstructionCoverageGap, InstructionGapKind,
        InstructionGapOrigin, InstructionIssueAssociation, InstructionObservationOwner,
        InstructionPathContext, InstructionSettlement, InstructionUnknownReason,
    };
    let fixture = Fixture::new();
    let (mut log, start) = PairedIssues::new(&fixture);
    let seq = log.projection.ticket(start.card).unwrap().unwrap().0.seq;
    let ticket = log
        .world
        .instructions
        .association_for_goal(start.goal.id, seq);
    let owner = InstructionObservationOwner::Managed {
        agent: start.agent,
        turn: TurnId::from_parts(30, 44),
        issue: InstructionIssueAssociation::Prospective,
    };
    let Record::InstructionObservationStarted { mut snapshot, .. } =
        samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!()
    };
    snapshot.gaps[0].agent = Some(start.agent);
    snapshot.execution_folder = None;
    snapshot.gaps[0].reason = InstructionGapKind::PathUnresolved;
    let binding = RepositoryInstructionBinding {
        hired_lsn: log.story.hired_lsn,
        ticket_start_lsn: Some(
            log.world
                .ticket_repository_round(start.goal.id)
                .unwrap()
                .start_lsn,
        ),
    };
    let observed = RepositoryInstructionStart {
        v: INSTRUCTION_BATCH_VERSION,
        binding,
        owner,
        ticket,
        members: log
            .story
            .hire
            .repositories
            .repositories
            .entries()
            .iter()
            .map(|member| RepositoryInstructionSnapshot {
                repo_id: member.repo_id.clone(),
                root: None,
                snapshot: snapshot.clone(),
            })
            .collect(),
    };
    let recorded = write_record(
        &mut log.writer,
        Record::InstructionObservationBatchStarted(observed.clone()),
    );
    apply_pair(&mut log.world, &mut log.projection, &recorded);
    let began = recorded.0;
    let settlement = InstructionSettlement::Unknown {
        reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
        end: None,
        sampled_at: None,
        recovery: true,
        comparison: InstructionComparison {
            changes: Vec::new(),
            gaps: vec![InstructionCoverageGap {
                origin: InstructionGapOrigin::Observed,
                reason: InstructionGapKind::UnrecordedLaunchBoundary,
                context: InstructionPathContext::SharedProject,
                agent: Some(start.agent),
                path: None,
            }],
            usage: dudes_proto::InstructionSampleUsage::default(),
        },
    };
    let recorded = write_record(
        &mut log.writer,
        Record::InstructionObservationBatchFinished(RepositoryInstructionFinish {
            v: INSTRUCTION_BATCH_VERSION,
            binding,
            owner,
            ticket,
            baseline: InstructionBaselineRef::Started { lsn: began },
            members: observed
                .members
                .iter()
                .map(|member| RepositoryInstructionSettlement {
                    repo_id: member.repo_id.clone(),
                    ending_root: None,
                    settlement: settlement.clone(),
                })
                .collect(),
        }),
    );
    apply_pair(&mut log.world, &mut log.projection, &recorded);
    log.append(Record::GoalCleared {
        agent: start.agent,
        goal: start.goal.id,
    });
    assert!(log.world.instructions.pending_for_goal(start.goal.id));
    assert_eq!(
        log.projection.ticket(start.card).unwrap().unwrap().0.status,
        CardStatus::Blocked
    );
    assert_eq!(log.world.agents[&start.agent].goal, None);
    assert_eq!(
        log.world
            .active_repository_round(start.agent)
            .unwrap()
            .unwrap()
            .goal,
        start.goal.id
    );
    let grant =
        RepositoryTurnGrant::select(log.story.hired_lsn, &log.story.hire.repositories, None)
            .unwrap();
    let record = Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
        grant,
        payload: legacy_issue(start.agent)
            .issue_payload()
            .unwrap()
            .to_owned(),
    }));
    let next = Lsn(log.projection.last_applied_lsn().0 + 1);
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    };
    assert!(matches!(
        log.projection.apply(next, 1, &entry),
        Err(dudes_journal::Error::Repository {
            reason: dudes_proto::repository_state::history::RepositoryStateError::Issue,
            ..
        })
    ));
    assert!(log.world.apply_checked(next, &entry).is_err());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one journal prefix proves restored trust retains both hire and issued authority"
)]
fn equivalent_retrust_restores_authority_without_rewriting_the_hire_or_issue() {
    let fixture = Fixture::new();
    let (mut log, start) = PairedIssues::new(&fixture);
    let decision = log.append(Record::LoopDecided {
        agent: start.agent,
        input_digest: "original".into(),
        gate_trace: trace(),
        decision: LoopDecision::Continue {
            task: "Keep the original task".into(),
        },
    });
    let turn = TurnId::from_parts(31, 1);
    log.issue(
        RecordKind::TurnIssued,
        start.goal.id,
        turn,
        "Keep the original task",
        json!({"lsn_of_decision": decision}),
    );
    let hired = log.world.agents[&start.agent].repositories.clone();
    let frozen = log.projection.turn(turn).unwrap().unwrap();
    let task = log.world.turns[&turn].task.clone();
    let original = log
        .projection
        .repository_trust()
        .unwrap()
        .into_iter()
        .find(|trust| trust.repo_id.as_str() == "docs")
        .unwrap();
    let pin = original.pin.clone().unwrap();
    let mut candidate = frozen
        .repositories
        .as_ref()
        .unwrap()
        .grant()
        .unwrap()
        .clone();
    assert_eq!(candidate.repositories.entries().len(), 2);
    assert!(hired.worktrees().count() > candidate.repositories.entries().len());
    let mut payload = legacy_issue(start.agent)
        .issue_payload()
        .unwrap()
        .to_owned();
    let dudes_proto::issue::ManagedIssuePayload::Ordinary {
        goal,
        turn: candidate_turn,
        ..
    } = &mut payload
    else {
        unreachable!()
    };
    *goal = start.goal.id;
    *candidate_turn = TurnId::from_parts(31, 2);
    let issue = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
            grant: candidate.clone(),
            payload,
        })),
    };
    let trust_record = |pin| Record::RepositoryTrustSet {
        project: original.project.clone(),
        repo_id: original.repo_id.clone(),
        catalog_lsn: original.catalog_lsn,
        origin: original.origin.clone(),
        pin,
    };
    for changed in [
        None,
        Some(dudes_proto::record::TrustPin {
            config: dudes_proto::record::Hex32::new([99; 32]),
            ..pin.clone()
        }),
    ] {
        let recorded = write_record(&mut log.writer, trust_record(changed));
        apply_pair(&mut log.world, &mut log.projection, &recorded);
        let next = Lsn(log.projection.last_applied_lsn().0 + 1);
        let before = log.projection.export().unwrap();
        assert!(matches!(
            log.projection.apply(next, 1, &issue),
            Err(dudes_journal::Error::Repository {
                reason: dudes_proto::repository_state::history::RepositoryStateError::Trust,
                ..
            })
        ));
        assert!(log.world.apply_checked(next, &issue).is_err());
        assert_eq!(log.projection.export().unwrap(), before);
    }
    let restored = write_record(&mut log.writer, trust_record(Some(pin)));
    assert_ne!(restored.0, original.lsn);
    apply_pair(&mut log.world, &mut log.projection, &restored);
    let recorded = write_record(&mut log.writer, issue.record);
    apply_pair(&mut log.world, &mut log.projection, &recorded);
    assert_eq!(log.world.agents[&start.agent].repositories, hired);
    assert_eq!(log.projection.turn(turn).unwrap().unwrap(), frozen);
    assert_eq!(log.world.turns[&turn].task, task);
    assert_eq!(log.world.turns[&turn].repositories, frozen.repositories);
    let HireRepositories::Scoped(current) = &log.world.agents[&start.agent].repositories else {
        panic!("scoped hire")
    };
    candidate = RepositoryTurnGrant::select(
        log.story.hired_lsn,
        current,
        log.world.active_repository_round(start.agent).unwrap(),
    )
    .unwrap();
    assert_eq!(
        candidate,
        *frozen.repositories.as_ref().unwrap().grant().unwrap()
    );
    let expected = log.projection.export().unwrap();
    drop(log);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut cold = Projections::in_memory().unwrap();
    replay(&reader, &mut cold).unwrap();
    assert_eq!(cold.export().unwrap(), expected);
}
