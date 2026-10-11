#![allow(missing_docs)]

use std::path::PathBuf;

use dudes_journal::{Error, Fsync, JournalOptions, Projections, Reader, Writer, open, replay};
use dudes_proto::mail_wake::MailWakeState;
use dudes_proto::record::{Hex32, samples};
use dudes_proto::{
    Entry, Gate, GateStep, LoopDecision, MailFrom, MailRef, MailTo, MailWakeAdmission,
    MailWakeDecision, MailWakeDigest, MailWakeDigestDomain, MailWakeIssue, MailWakePlan,
    MailWakePurpose, MailWakeTarget, MailWakeTask, Record, RecordKind,
};
use dudes_types::{
    AgentId, GoalId, GoalStatus, Lsn, MailId, MailState, Mode, ModelSelection, Timestamp, TurnId,
};

struct Log {
    owned: tempfile::TempDir,
    journal: PathBuf,
    writer: Option<Writer>,
    agent: AgentId,
    goal: GoalId,
    completion: Lsn,
    peer_context: Lsn,
    receipts: Vec<MailRef>,
    counts: Vec<u8>,
}

impl Log {
    fn new(counts: &[u8]) -> Self {
        let owned = tempfile::tempdir().unwrap();
        let journal = owned.path().join("journal");
        let writer = open(
            &journal,
            JournalOptions {
                fsync: Fsync::None,
                ..JournalOptions::default()
            },
        )
        .unwrap()
        .0;
        let hired = samples::record(RecordKind::AgentHired);
        let Record::AgentHired { id: agent, .. } = hired else {
            unreachable!();
        };
        let goal_record = samples::record(RecordKind::GoalSet);
        let Record::GoalSet { goal, .. } = &goal_record else {
            unreachable!();
        };
        let goal = goal.id;
        let mut log = Self {
            owned,
            journal,
            writer: Some(writer),
            agent,
            goal,
            completion: Lsn::ZERO,
            peer_context: Lsn::ZERO,
            receipts: Vec::new(),
            counts: counts.to_vec(),
        };
        log.append(samples::record(RecordKind::AgentHired), stamp());
        let mut peer = samples::record(RecordKind::AgentHired);
        let Record::AgentHired { id, .. } = &mut peer else {
            unreachable!();
        };
        *id = AgentId::from_parts(1, 90);
        log.peer_context = log.append(peer, stamp());
        log.append(goal_record, stamp());
        log.append(samples::record(RecordKind::PredicateResult), stamp());
        log.completion = log.append(
            Record::LoopDecided {
                agent,
                input_digest: "ordinary-done".into(),
                gate_trace: Vec::new(),
                decision: LoopDecision::Done,
            },
            stamp(),
        );
        for (index, count) in counts.iter().enumerate() {
            let mut record = samples::record(RecordKind::Mail);
            let Record::Mail {
                id,
                from,
                to,
                state,
                ..
            } = &mut record
            else {
                unreachable!();
            };
            *id = MailId::from_parts(1, u128::try_from(index).unwrap() + 100);
            let id = *id;
            *from = MailFrom::Agent {
                agent: AgentId::from_parts(1, 90),
            };
            *to = MailTo::Agent { agent };
            *state = MailState::Durable;
            let lsn = log.append(record, stamp());
            log.receipts.push(MailRef { id, lsn });
            if *count != 0 {
                log.append(
                    Record::MailState {
                        id,
                        state: MailState::Notified { n: *count },
                    },
                    stamp(),
                );
            }
        }
        log
    }

    fn append(&mut self, record: Record, at: Timestamp) -> Lsn {
        self.writer
            .as_mut()
            .unwrap()
            .append(&Entry { at, record })
            .unwrap()
    }

    fn root_record(&self) -> Record {
        let task = MailWakeTask::new(u8::try_from(self.receipts.len()).unwrap()).unwrap();
        Record::MailWakeTurnDecided(MailWakeDecision {
            agent: self.agent,
            goal: self.goal,
            completion_lsn: self.completion,
            purpose: MailWakePurpose::FreshPreparation {
                plan: MailWakePlan {
                    trigger: self.receipts[0],
                    frontier: self.receipts.last().unwrap().lsn,
                    unread_count: u8::try_from(self.receipts.len()).unwrap(),
                    task: task.clone(),
                    targets: self
                        .receipts
                        .iter()
                        .zip(&self.counts)
                        .filter(|(_, count)| **count < 5)
                        .map(|(mail, count)| MailWakeTarget {
                            mail: *mail,
                            original_count: *count,
                            target_count: *count + 1,
                        })
                        .collect(),
                },
            },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Fresh,
                value: Hex32::new([1; 32]),
            },
            gate_trace: trace(),
            decision: MailWakeAdmission::Issue { task },
        })
    }

    fn resume_record(&self, root_lsn: Lsn) -> Record {
        Record::MailWakeTurnDecided(MailWakeDecision {
            agent: self.agent,
            goal: self.goal,
            completion_lsn: self.completion,
            purpose: MailWakePurpose::ExistingPreparation { root_lsn },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Resume,
                value: Hex32::new([2; 32]),
            },
            gate_trace: trace(),
            decision: MailWakeAdmission::Issue {
                task: MailWakeTask::new(u8::try_from(self.receipts.len()).unwrap()).unwrap(),
            },
        })
    }

    fn advance(&mut self, index: usize) -> Lsn {
        self.append(
            Record::MailState {
                id: self.receipts[index].id,
                state: MailState::Notified {
                    n: self.counts[index] + 1,
                },
            },
            stamp(),
        )
    }

    fn issue_record(&self, root_lsn: Lsn, lsn_of_decision: Lsn) -> Record {
        Record::MailWakeOnlyTurnIssued(MailWakeIssue {
            agent: self.agent,
            goal: self.goal,
            turn: TurnId::from_parts(1, 91),
            completion_lsn: self.completion,
            root_lsn,
            lsn_of_decision,
            task: MailWakeTask::new(u8::try_from(self.receipts.len()).unwrap()).unwrap(),
            session: None,
            mode: Mode::Managed,
            model_selection: ModelSelection::default(),
            answers: Vec::new(),
        })
    }

    fn reader(&mut self) -> Reader {
        self.writer.take();
        Reader::open(&self.journal).unwrap().0
    }

    fn new_receipt(&mut self, index: u128, at: Timestamp) {
        let mut record = samples::record(RecordKind::Mail);
        let Record::Mail {
            id,
            from,
            to,
            state,
            ..
        } = &mut record
        else {
            unreachable!();
        };
        *id = MailId::from_parts(1, index + 100);
        let id = *id;
        *from = MailFrom::Agent {
            agent: AgentId::from_parts(1, 90),
        };
        *to = MailTo::Agent { agent: self.agent };
        *state = MailState::Durable;
        let lsn = self.append(record, at);
        self.receipts = vec![MailRef { id, lsn }];
        self.counts = vec![0];
    }

    fn finish_turn(&mut self, turn: TurnId, at: Timestamp) {
        let mut record = samples::record(RecordKind::TurnEnded);
        let Record::TurnEnded {
            agent,
            turn: ended,
            usage,
            model_usage,
            elapsed_ms,
            session,
            session_usage,
            outcome,
            exit,
            ..
        } = &mut record
        else {
            unreachable!();
        };
        *agent = self.agent;
        *ended = turn;
        *usage = Some(dudes_types::Usage::ZERO);
        model_usage.clear();
        *elapsed_ms = Some(10_000);
        *session = None;
        *session_usage = None;
        *outcome = Some(dudes_proto::TurnOutcome::Completed);
        *exit = dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        };
        self.append(record, at);
    }

    fn finish_issue(&mut self, turn: TurnId, at: Timestamp) {
        self.finish_turn(turn, at);
        self.append(
            Record::MailState {
                id: self.receipts[0].id,
                state: MailState::Acked,
            },
            at,
        );
    }
}

fn stamp() -> Timestamp {
    Timestamp::from_unix_millis(1_787_394_600_000)
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

#[test]
fn pending_index_preserves_ownership_terminal_identity_and_transaction_rollback() {
    let mut log = Log::new(&[4, 5]);
    let first = log.receipts[0].id;
    let second = log.receipts[1].id;
    let root = log.append(log.root_record(), stamp());
    let mut projections = Projections::in_memory().unwrap();
    replay(&Reader::new(&log.journal), &mut projections).unwrap();
    let before = projections.export().unwrap();
    assert_eq!(
        before
            .lines()
            .filter(|line| line.starts_with("mail_pending "))
            .count(),
        2
    );
    let terminal = Entry {
        at: stamp(),
        record: Record::MailState {
            id: first,
            state: MailState::Acked,
        },
    };
    let invalid = Entry {
        at: stamp(),
        record: log.root_record(),
    };
    assert!(
        projections
            .apply_batch([
                (Lsn(root.0 + 1), 0, &terminal),
                (Lsn(root.0 + 2), 0, &invalid),
            ])
            .is_err()
    );
    assert_eq!(projections.export().unwrap(), before);
    log.append(terminal.record, stamp());
    log.append(
        Record::MailState {
            id: second,
            state: MailState::Bounced {
                why: "closed".into(),
            },
        },
        stamp(),
    );
    let mut duplicate = samples::record(RecordKind::Mail);
    let Record::Mail { id, to, state, .. } = &mut duplicate else {
        unreachable!()
    };
    *id = first;
    *to = MailTo::Agent {
        agent: AgentId::from_parts(1, 90),
    };
    *state = MailState::Durable;
    log.append(duplicate, stamp());
    log.append(
        Record::MailState {
            id: MailId::from_parts(9, 9),
            state: MailState::Durable,
        },
        stamp(),
    );
    let reader = log.reader();
    replay(&reader, &mut projections).unwrap();
    let exported = projections.export().unwrap();
    assert_eq!(
        exported
            .lines()
            .filter(|line| line.starts_with("mail_pending "))
            .count(),
        0
    );
    assert_eq!(
        exported
            .lines()
            .filter(|line| line.starts_with("mail "))
            .count(),
        2
    );
    assert_eq!(projections.unread_by_agent(log.agent).unwrap(), 0);
    assert_eq!(
        projections
            .unread_by_agent(AgentId::from_parts(1, 90))
            .unwrap(),
        0
    );
    let mut rebuilt = Projections::in_memory().unwrap();
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), exported);
}

#[test]
fn version_36_replays_to_rebuild_pending_recipient_membership() {
    let mut log = Log::new(&[0, 5]);
    log.append(log.root_record(), stamp());
    let reader = log.reader();
    let cache = log.owned.path().join("state.redb");
    let mut projections = Projections::open(&cache).unwrap();
    replay(&reader, &mut projections).unwrap();
    let expected = projections.export().unwrap();
    drop(projections);
    let db = redb::Database::create(&cache).unwrap();
    let txn = db.begin_write().unwrap();
    txn.delete_table(redb::TableDefinition::<&[u8; 32], &[u8]>::new(
        "mail_pending",
    ))
    .unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("meta"))
        .unwrap()
        .insert("schema_version", &36)
        .unwrap();
    txn.commit().unwrap();
    drop(db);
    let mut rebuilt = Projections::open(&cache).unwrap();
    assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), expected);
}
fn state(projections: &Projections, agent: AgentId) -> MailWakeState {
    use std::fmt::Write as _;
    let mut prefix = "mail_wakes ".to_owned();
    for byte in agent.to_bytes() {
        write!(&mut prefix, "{byte:02x}").unwrap();
    }
    prefix.push(' ');
    let export = projections.export().unwrap();
    serde_json::from_str(
        export
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap(),
    )
    .unwrap()
}

fn completed_note(log: &mut Log, taught: bool, turn: TurnId) {
    let text = if taught { "taught input" } else { "raw input" };
    let mut note = samples::record(RecordKind::BrowserNoteQueued);
    let Record::BrowserNoteQueued {
        agent,
        text: queued,
        destination,
        ..
    } = &mut note
    else {
        unreachable!()
    };
    *agent = log.agent;
    *queued = text.into();
    *destination = dudes_proto::BrowserNoteDestination::ManagedGoal { goal: log.goal };
    let note_lsn = log.append(note, stamp());
    let task = if taught {
        dudes_proto::taught_note_task(text).unwrap()
    } else {
        text.to_owned()
    };
    let decided = if taught {
        Record::TaughtNoteTurnDecided {
            agent: log.agent,
            goal: log.goal,
            note_lsn,
            completion_lsn: log.completion,
            input_digest: "taught current-library control".into(),
            gate_trace: trace(),
            decision: dudes_proto::NoteAdmission::Issue { task: task.clone() },
        }
    } else {
        Record::NoteTurnDecided {
            agent: log.agent,
            goal: log.goal,
            note_lsn,
            completion_lsn: log.completion,
            input_digest: "raw current-library control".into(),
            gate_trace: Vec::new(),
            decision: dudes_proto::NoteAdmission::Issue { task: task.clone() },
        }
    };
    let lsn_of_decision = log.append(decided, stamp());
    let issued: Record = serde_json::from_value(serde_json::json!({
        "t": if taught { "taught_note_only_turn_issued" } else { "note_only_turn_issued" },
        "agent": log.agent, "goal": log.goal, "turn": turn, "note_lsn": note_lsn,
        "completion_lsn": log.completion, "lsn_of_decision": lsn_of_decision,
        "task": task, "mode": "managed", "session": null, "model_selection": {}, "answers": []
    }))
    .unwrap();
    log.append(issued, stamp());
    log.finish_turn(turn, stamp());
}

fn conversation(log: &mut Log, turn: TurnId) {
    let peer = AgentId::from_parts(1, 90);
    let note_lsn = log.append(
        Record::ConversationMessageQueued {
            agent: peer,
            text: "finite conversation".into(),
            browser: dudes_proto::BrowserNoteAttribution {
                request_id_digest: Hex32::new([1; 32]),
                request_digest: Hex32::new([2; 32]),
                session_digest: Hex32::new([3; 32]),
            },
            context_lsn: log.peer_context,
            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
        },
        stamp(),
    );
    let decision = log.append(
        Record::ConversationTurnDecided {
            agent: peer,
            note_lsn,
            context_lsn: log.peer_context,
            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
            input_digest: "finite conversation control".into(),
            gate_trace: trace(),
            decision: dudes_proto::NoteAdmission::Issue {
                task: "finite conversation".into(),
            },
        },
        stamp(),
    );
    log.append(
        Record::ConversationTurnIssued {
            agent: peer,
            turn,
            note_lsn,
            context_lsn: log.peer_context,
            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
            task: "finite conversation".into(),
            session: None,
            model_selection: ModelSelection::default(),
            lsn_of_decision: decision,
        },
        stamp(),
    );
    let mut ended = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent,
        turn: ended_turn,
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
    *agent = peer;
    *ended_turn = turn;
    *usage = Some(dudes_types::Usage::ZERO);
    model_usage.clear();
    *session = None;
    *session_usage = None;
    *outcome = Some(dudes_proto::TurnOutcome::Completed);
    log.append(ended, stamp());
}

#[test]
fn current_conversation_raw_taught_and_wake_history_rebuilds_cache_35_to_38() {
    assert_eq!(dudes_journal::PROJECTION_VERSION, 45);
    let mut log = Log::new(&[0]);
    let conversation_turn = TurnId::from_parts(3, 1);
    let raw_turn = TurnId::from_parts(3, 2);
    let taught_turn = TurnId::from_parts(3, 3);
    conversation(&mut log, conversation_turn);
    completed_note(&mut log, false, raw_turn);
    completed_note(&mut log, true, taught_turn);
    let root = log.append(log.root_record(), stamp());
    log.advance(0);
    let decision = log.append(log.resume_record(root), stamp());
    log.append(log.issue_record(root, decision), stamp());
    let reader = log.reader();
    let mut fresh = Projections::in_memory().unwrap();
    replay(&reader, &mut fresh).unwrap();
    assert_eq!(fresh.turn(conversation_turn).unwrap().unwrap().goal, None);
    for turn in [raw_turn, taught_turn, TurnId::from_parts(1, 91)] {
        assert_eq!(fresh.turn(turn).unwrap().unwrap().goal, Some(log.goal));
    }
    assert_eq!(fresh.goal(log.goal).unwrap().unwrap().turns_on_goal, 3);
    assert_eq!(
        fresh.goal(log.goal).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    assert_eq!(fresh.agent(log.agent).unwrap().unwrap().turns, 3);
    let peer = fresh.agent(AgentId::from_parts(1, 90)).unwrap().unwrap();
    assert_eq!(peer.turns, 1);
    assert_eq!(peer.goal, None);
    assert_eq!(peer.context_lsn, log.peer_context);
    let expected = fresh.export().unwrap();
    let cache = log.owned.path().join("state.redb");
    let mut cached = Projections::open(&cache).unwrap();
    cached
        .apply(
            Lsn(u64::MAX - 1),
            17,
            &Entry {
                at: stamp(),
                record: samples::record(RecordKind::Checkpoint),
            },
        )
        .unwrap();
    drop(cached);
    let database = redb::Database::create(&cache).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("meta"))
        .unwrap()
        .insert("schema_version", &35)
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let mut rebuilt = Projections::open(&cache).unwrap();
    assert_eq!(rebuilt.last_applied_lsn(), Lsn::ZERO);
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), expected);
}

#[test]
fn goal_clear_set_and_reassign_retire_unissued_wake_and_preserve_context_revision() {
    for change in 0..3 {
        let mut log = Log::new(&[0]);
        log.append(log.root_record(), stamp());
        log.advance(0);
        let record = match change {
            0 => Record::GoalCleared {
                agent: log.agent,
                goal: log.goal,
            },
            1 => {
                let mut record = samples::record(RecordKind::GoalSet);
                let Record::GoalSet { agent, goal, .. } = &mut record else {
                    unreachable!()
                };
                *agent = log.agent;
                goal.id = GoalId::from_parts(9, 9);
                record
            }
            _ => Record::GoalReassigned {
                goal: log.goal,
                from: log.agent,
                to: AgentId::from_parts(1, 90),
                reason: None,
            },
        };
        let revision = log.append(record, stamp());
        let reader = log.reader();
        let mut projected = Projections::in_memory().unwrap();
        replay(&reader, &mut projected).unwrap();
        let row = state(&projected, log.agent);
        assert!(row.preparation.is_none());
        assert_eq!(row.covered_frontier, log.receipts[0].lsn);
        assert_eq!(
            projected.agent(log.agent).unwrap().unwrap().context_lsn,
            revision
        );
        assert_eq!(projected.goal(log.goal).unwrap().unwrap().turns_on_goal, 0);
        if change == 2 {
            assert_eq!(
                projected
                    .agent(AgentId::from_parts(1, 90))
                    .unwrap()
                    .unwrap()
                    .context_lsn,
                revision
            );
        }
    }
}

/// A ticket's start installs a goal, so it retires an unissued wake exactly as a `GoalSet` does.
#[test]
fn mail_wake_projection_sees_ticket_start() {
    let mut log = Log::new(&[0]);
    log.append(log.root_record(), stamp());
    log.advance(0);
    let mut start = samples::record(RecordKind::TicketStarted);
    let Record::TicketStarted { agent, goal, .. } = &mut start else {
        unreachable!()
    };
    *agent = log.agent;
    goal.id = GoalId::from_parts(9, 9);
    let revision = log.append(start, stamp());
    let reader = log.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    let row = state(&projected, log.agent);
    assert!(row.preparation.is_none());
    assert!(row.candidate.is_none());
    assert_eq!(
        projected.agent(log.agent).unwrap().unwrap().context_lsn,
        revision
    );
}

#[test]
fn issued_wake_keeps_original_goal_when_current_owner_changes() {
    let mut log = Log::new(&[0]);
    let root = log.append(log.root_record(), stamp());
    log.advance(0);
    let decision = log.append(log.resume_record(root), stamp());
    log.append(log.issue_record(root, decision), stamp());
    let revision = log.append(
        Record::GoalReassigned {
            goal: log.goal,
            from: log.agent,
            to: AgentId::from_parts(1, 90),
            reason: None,
        },
        stamp(),
    );
    let reader = log.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    assert_eq!(
        projected
            .turn(TurnId::from_parts(1, 91))
            .unwrap()
            .unwrap()
            .goal,
        Some(log.goal)
    );
    assert_eq!(projected.goal(log.goal).unwrap().unwrap().turns_on_goal, 1);
    assert_eq!(projected.agent(log.agent).unwrap().unwrap().goal, None);
    assert_eq!(
        projected.agent(log.agent).unwrap().unwrap().context_lsn,
        revision
    );
    assert_eq!(
        projected
            .agent(AgentId::from_parts(1, 90))
            .unwrap()
            .unwrap()
            .goal,
        Some(log.goal)
    );
}

#[test]
fn each_canonical_preparation_prefix_rebuilds_without_another_charge() {
    for cut in 0..=5 {
        let mut log = Log::new(&[0, 0]);
        let root = if cut > 0 {
            Some(log.append(log.root_record(), stamp()))
        } else {
            None
        };
        let first = if cut > 1 { Some(log.advance(0)) } else { None };
        let second = if cut > 2 { Some(log.advance(1)) } else { None };
        let decision = if cut > 3 {
            Some(log.append(log.resume_record(root.unwrap()), stamp()))
        } else {
            None
        };
        if cut > 4 {
            log.append(log.issue_record(root.unwrap(), decision.unwrap()), stamp());
        }
        let reader = log.reader();
        let mut projected = Projections::in_memory().unwrap();
        let report = replay(&reader, &mut projected).unwrap();
        assert!(report.applied >= 7);
        let row = state(&projected, log.agent);
        assert_eq!(
            projected.goal(log.goal).unwrap().unwrap().status,
            GoalStatus::Achieved
        );
        assert_eq!(
            projected.goal(log.goal).unwrap().unwrap().turns_on_goal,
            u32::from(cut == 5)
        );
        assert_eq!(
            projected.agent(log.agent).unwrap().unwrap().turns,
            u64::from(cut == 5)
        );
        if cut == 0 {
            assert_eq!(row.candidate.as_ref().unwrap().trigger, log.receipts[0]);
            assert_eq!(
                row.candidate.as_ref().unwrap().frontier,
                log.receipts[1].lsn
            );
        } else if cut < 5 {
            let prepared = row.preparation.as_ref().unwrap();
            assert_eq!(prepared.root_lsn, root.unwrap());
            assert_eq!(
                prepared.progress[0],
                first.map_or(dudes_proto::mail_wake::MailWakeProgress::Pending, |lsn| {
                    dudes_proto::mail_wake::MailWakeProgress::Notified { lsn }
                })
            );
            assert_eq!(
                prepared.progress[1],
                second.map_or(dudes_proto::mail_wake::MailWakeProgress::Pending, |lsn| {
                    dudes_proto::mail_wake::MailWakeProgress::Notified { lsn }
                })
            );
        } else {
            assert!(row.preparation.is_none());
            assert_eq!(row.covered_frontier, log.receipts[1].lsn);
        }
        let expected = projected.export().unwrap();
        assert_eq!(replay(&reader, &mut projected).unwrap().applied, 0);
        assert_eq!(projected.export().unwrap(), expected);
        let mut fresh = Projections::in_memory().unwrap();
        assert_eq!(replay(&reader, &mut fresh).unwrap().applied, report.applied);
        assert_eq!(fresh.export().unwrap(), expected);
    }
}

#[test]
fn invalid_root_prefix_is_refused_before_torn_repair_and_advanced_cache_apply() {
    use std::io::Write;
    let mut log = Log::new(&[0]);
    let mut bad = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut bad else {
        unreachable!();
    };
    decision.goal = GoalId::from_parts(1, 999);
    let bad_lsn = log.append(bad, stamp());
    log.writer.take();
    let tail_path = std::fs::read_dir(&log.journal)
        .unwrap()
        .map(|value| value.unwrap().path())
        .filter(|path| path.extension().is_some_and(|value| value == "log"))
        .max()
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&tail_path)
        .unwrap()
        .write_all(b"\0\xff")
        .unwrap();
    let before = std::fs::read(&tail_path).unwrap();
    let cache = log.owned.path().join("state.redb");
    std::fs::write(&cache, b"owned untouched cache witness").unwrap();
    assert!(
        matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == bad_lsn)
    );
    assert!(
        matches!(open(&log.journal, JournalOptions { fsync: Fsync::None, ..JournalOptions::default() }), Err(Error::MailWake { lsn, .. }) if lsn == bad_lsn)
    );
    assert_eq!(std::fs::read(&tail_path).unwrap(), before);
    assert_eq!(
        std::fs::read(&cache).unwrap(),
        b"owned untouched cache witness"
    );
    let mut advanced = Projections::in_memory().unwrap();
    advanced
        .apply(
            Lsn(u64::MAX - 1),
            17,
            &Entry {
                at: stamp(),
                record: samples::record(RecordKind::Checkpoint),
            },
        )
        .unwrap();
    let expected = advanced.export().unwrap();
    assert!(
        matches!(replay(&Reader::new(&log.journal), &mut advanced), Err(Error::MailWake { lsn, .. }) if lsn == bad_lsn)
    );
    assert_eq!(advanced.export().unwrap(), expected);
}

#[test]
fn contracts_are_validated_only_from_the_first_segment_so_an_archived_prefix_still_opens() {
    let mut log = Log::new(&[0]);
    let mut bad = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut bad else {
        unreachable!();
    };
    decision.goal = GoalId::from_parts(1, 999);
    let bad_lsn = log.append(bad, stamp());
    log.writer.take();
    assert!(
        matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == bad_lsn)
    );
    std::fs::remove_file(dudes_journal::segment::segment_path(&log.journal, 1)).unwrap();
    assert!(Reader::open(&log.journal).is_ok());
}

#[test]
fn retired_terminal_prefix_refuses_stale_notification_before_repair_and_cache() {
    use std::io::Write;
    let mut log = Log::new(&[0]);
    log.append(log.root_record(), stamp());
    let id = log.receipts[0].id;
    log.append(
        Record::MailState {
            id,
            state: MailState::Acked,
        },
        stamp(),
    );
    let mut positive = Projections::in_memory().unwrap();
    replay(&Reader::new(&log.journal), &mut positive).unwrap();
    let retired = state(&positive, log.agent);
    assert!(retired.preparation.is_none());
    assert_eq!(retired.covered_frontier, log.receipts[0].lsn);
    assert_eq!(
        positive.messages().unwrap()[0].state,
        dudes_proto::MessageState::Acked
    );
    assert_eq!(
        positive.goal(log.goal).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    assert_eq!(positive.agent(log.agent).unwrap().unwrap().turns, 0);
    let stale = log.append(
        Record::MailState {
            id,
            state: MailState::Notified { n: 1 },
        },
        stamp(),
    );
    log.writer.take();
    let tail = std::fs::read_dir(&log.journal)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|value| value == "log"))
        .max()
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&tail)
        .unwrap()
        .write_all(b"\0\xff")
        .unwrap();
    let before = std::fs::read(&tail).unwrap();
    let cache = log.owned.path().join("state.redb");
    std::fs::write(&cache, b"owned terminal cache witness").unwrap();
    let read = Reader::open(&log.journal);
    let reader_refused = matches!(&read, Err(Error::MailWake { lsn, .. }) if *lsn == stale);
    let overwritten = read.as_ref().ok().map(|(reader, _)| {
        let mut replayed = Projections::in_memory().unwrap();
        replay(reader, &mut replayed).unwrap();
        replayed.messages().unwrap()[0].state
    });
    let mut advanced = Projections::in_memory().unwrap();
    advanced
        .apply(
            Lsn(u64::MAX - 1),
            17,
            &Entry {
                at: stamp(),
                record: samples::record(RecordKind::Checkpoint),
            },
        )
        .unwrap();
    let advanced_before = advanced.export().unwrap();
    let replay_result = replay(&Reader::new(&log.journal), &mut advanced);
    let advanced_refused =
        matches!(&replay_result, Err(Error::MailWake { lsn, .. }) if *lsn == stale);
    assert_eq!(advanced.export().unwrap(), advanced_before);
    let opened = open(
        &log.journal,
        JournalOptions {
            fsync: Fsync::None,
            ..JournalOptions::default()
        },
    );
    let writer_refused = matches!(&opened, Err(Error::MailWake { lsn, .. }) if *lsn == stale);
    let tail_unchanged = std::fs::read(&tail).unwrap() == before;
    assert_eq!(
        std::fs::read(&cache).unwrap(),
        b"owned terminal cache witness"
    );
    assert!(
        reader_refused && writer_refused && advanced_refused && tail_unchanged,
        "retired terminal must refuse before effects: reader={reader_refused} writer={writer_refused} advanced={advanced_refused} tail_unchanged={tail_unchanged} replayed_state={overwritten:?} advanced_result={replay_result:?}"
    );
}

#[test]
fn covered_nonterminal_receipts_refuse_late_notifications_after_retire_or_issue() {
    for change in 0..3 {
        let mut log = Log::new(&[0]);
        let root = log.append(log.root_record(), stamp());
        if change == 0 {
            log.append(
                Record::GoalCleared {
                    agent: log.agent,
                    goal: log.goal,
                },
                stamp(),
            );
        } else if change == 1 {
            log.append(
                Record::AgentRetired {
                    id: log.agent,
                    reason: "finished".into(),
                    browser_retire: None,
                },
                stamp(),
            );
        } else {
            log.advance(0);
            let decision = log.append(log.resume_record(root), stamp());
            log.append(log.issue_record(root, decision), stamp());
        }
        let mut positive = Projections::in_memory().unwrap();
        replay(&Reader::new(&log.journal), &mut positive).unwrap();
        let covered = state(&positive, log.agent);
        assert!(covered.preparation.is_none());
        assert_eq!(covered.covered_frontier, log.receipts[0].lsn);
        assert!(matches!(
            positive.messages().unwrap()[0].state,
            dudes_proto::MessageState::Durable | dudes_proto::MessageState::Notified
        ));
        let stale = log.append(
            Record::MailState {
                id: log.receipts[0].id,
                state: MailState::Notified {
                    n: if change == 2 { 2 } else { 1 },
                },
            },
            stamp(),
        );
        log.writer.take();
        assert!(
            matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == stale)
        );
    }
}

#[test]
fn forty_pending_mails_replay_one_plan_of_the_oldest_thirty_two() {
    let mut log = Log::new(&[0; 40]);
    let frontier = log.receipts.last().unwrap().lsn;
    let oldest: Vec<_> = log.receipts[..32].iter().map(|mail| mail.id).collect();
    log.receipts.truncate(32);
    let mut root = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut root else {
        unreachable!();
    };
    let MailWakePurpose::FreshPreparation { plan } = &mut decision.purpose else {
        unreachable!();
    };
    plan.frontier = frontier;
    let root_lsn = log.append(root, stamp());
    let mut replayed = Projections::in_memory().unwrap();
    replay(&Reader::new(&log.journal), &mut replayed).unwrap();
    let preparation = state(&replayed, log.agent).preparation.unwrap();
    assert_eq!(preparation.root_lsn, root_lsn);
    let planned: Vec<_> = preparation.plan.targets.iter().map(|t| t.mail.id).collect();
    assert_eq!(planned, oldest);
}

#[test]
fn receipt_context_keeps_eligible_mail_after_thirty_two_fifth_notifications() {
    let mut counts = vec![5; 32];
    counts.extend([0; 8]);
    let mut log = Log::new(&counts);
    let trigger = log.receipts[0];
    let frontier = log.receipts.last().unwrap().lsn;
    let targets: Vec<_> = log.receipts[32..].iter().map(|mail| mail.id).collect();
    log.receipts.drain(..32);
    log.counts.drain(..32);
    let mut root = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut root else {
        unreachable!()
    };
    let MailWakePurpose::FreshPreparation { plan } = &mut decision.purpose else {
        unreachable!()
    };
    let task = MailWakeTask::new(32).unwrap();
    plan.trigger = trigger;
    plan.frontier = frontier;
    plan.unread_count = 32;
    plan.task = task.clone();
    decision.decision = MailWakeAdmission::Issue { task };
    log.append(root, stamp());
    let mut projections = Projections::in_memory().unwrap();
    replay(&log.reader(), &mut projections).unwrap();
    let preparation = state(&projections, log.agent).preparation.unwrap();
    assert_eq!(preparation.plan.unread_count, 32);
    assert_eq!(
        preparation
            .plan
            .targets
            .iter()
            .map(|target| target.mail.id)
            .collect::<Vec<_>>(),
        targets
    );
    assert_eq!(projections.unread_by_agent(log.agent).unwrap(), 40);
}

#[test]
fn later_preparation_reuses_older_eligible_target_before_covered_frontier() {
    let mut log = Log::new(&[0]);
    let old = log.receipts[0];
    let first = log.append(log.root_record(), stamp());
    log.advance(0);
    let decision = log.append(log.resume_record(first), stamp());
    log.append(log.issue_record(first, decision), stamp());
    log.finish_turn(TurnId::from_parts(1, 91), stamp());
    let mut ended = Projections::in_memory().unwrap();
    replay(&Reader::new(&log.journal), &mut ended).unwrap();
    assert_eq!(
        ended.messages().unwrap()[0].state,
        dudes_proto::MessageState::Notified
    );
    assert!(ended.agent(log.agent).unwrap().unwrap().open_turn.is_none());
    log.new_receipt(1, stamp());
    let fresh = log.receipts[0];
    log.receipts = vec![old, fresh];
    log.counts = vec![1, 0];
    let mut root = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut root else {
        unreachable!();
    };
    let MailWakePurpose::FreshPreparation { plan } = &mut decision.purpose else {
        unreachable!();
    };
    plan.trigger = fresh;
    let second = log.append(root, stamp());
    let older_advance = log.advance(0);
    log.advance(1);
    let mut positive = Projections::in_memory().unwrap();
    replay(&Reader::new(&log.journal), &mut positive).unwrap();
    let row = state(&positive, log.agent);
    assert_eq!(row.covered_frontier, old.lsn);
    let active = row.preparation.unwrap();
    assert_eq!(active.root_lsn, second);
    assert_eq!(active.plan.targets[0].mail, old);
    assert_eq!(active.plan.targets[0].original_count, 1);
    assert_eq!(active.plan.targets[0].target_count, 2);
    assert_eq!(
        active.progress[0],
        dudes_proto::mail_wake::MailWakeProgress::Notified { lsn: older_advance }
    );
    let decision = log.append(log.resume_record(second), stamp());
    let mut issue = log.issue_record(second, decision);
    let Record::MailWakeOnlyTurnIssued(carrier) = &mut issue else {
        unreachable!();
    };
    carrier.turn = TurnId::from_parts(1, 92);
    log.append(issue, stamp());
    let reader = log.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    assert_eq!(projected.agent(log.agent).unwrap().unwrap().turns, 2);
    assert_eq!(projected.goal(log.goal).unwrap().unwrap().turns_on_goal, 2);
    assert_eq!(
        projected.goal(log.goal).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    assert_eq!(state(&projected, log.agent).covered_frontier, fresh.lsn);
}

#[test]
fn duplicate_advance_and_unconsumed_fifth_cannot_admit_an_issue() {
    for fifth in [false, true] {
        let mut log = Log::new(&[if fifth { 4 } else { 0 }]);
        let root = log.append(log.root_record(), stamp());
        log.advance(0);
        let expected = if fifth {
            log.append(log.resume_record(root), stamp())
        } else {
            log.advance(0)
        };
        log.writer.take();
        assert!(
            matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == expected)
        );
    }
    let mut control = Log::new(&[0]);
    let root = control.append(control.root_record(), stamp());
    control.advance(0);
    let decision = control.append(control.resume_record(root), stamp());
    control.append(control.issue_record(root, decision), stamp());
    let reader = control.reader();
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    assert_eq!(projections.agent(control.agent).unwrap().unwrap().turns, 1);
}

#[test]
fn wake_payload_requires_its_exact_required_header() {
    for (version, kind) in [(1, 65), (2, 6), (1, u16::MAX)] {
        let mut log = Log::new(&[0]);
        let record = log.root_record();
        let root = log.append(record.clone(), stamp());
        log.writer.take();
        let path = std::fs::read_dir(&log.journal)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|value| value == "log"))
            .max()
            .unwrap();
        let payload = serde_json::to_vec(&Entry {
            at: stamp(),
            record,
        })
        .unwrap();
        let mut bytes = Vec::new();
        if version == 1 {
            dudes_journal::frame::encode(kind, &payload, &mut bytes).unwrap();
        } else {
            dudes_journal::frame::encode_required(RecordKind::TurnIssued, &payload, &mut bytes)
                .unwrap();
        }
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Reader::open(&log.journal), Err(Error::MailWakeFrame { lsn, .. }) if lsn == root)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let mut control = Log::new(&[0]);
    control.append(control.root_record(), stamp());
    let reader = control.reader();
    let mut projections = Projections::in_memory().unwrap();
    replay(&reader, &mut projections).unwrap();
    assert!(state(&projections, control.agent).preparation.is_some());
}

#[test]
fn terminal_targets_keep_snapshot_and_all_terminal_retirement_keeps_later_arrival() {
    let mut partial = Log::new(&[0, 0]);
    let root = partial.append(partial.root_record(), stamp());
    partial.append(
        Record::MailState {
            id: partial.receipts[0].id,
            state: MailState::Acked,
        },
        stamp(),
    );
    partial.advance(1);
    let decision = partial.append(partial.resume_record(root), stamp());
    let issue = partial.issue_record(root, decision);
    let Record::MailWakeOnlyTurnIssued(carrier) = &issue else {
        unreachable!();
    };
    assert_eq!(carrier.task.unread_count(), 2);
    partial.append(issue, stamp());
    let reader = partial.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    assert_eq!(projected.agent(partial.agent).unwrap().unwrap().turns, 1);
    assert!(state(&projected, partial.agent).preparation.is_none());

    let mut retired = Log::new(&[0, 0]);
    retired.append(retired.root_record(), stamp());
    let mut later = samples::record(RecordKind::Mail);
    let Record::Mail {
        id,
        to,
        state: delivery,
        ..
    } = &mut later
    else {
        unreachable!();
    };
    *id = MailId::from_parts(1, 999);
    let id = *id;
    *to = MailTo::Agent {
        agent: retired.agent,
    };
    *delivery = MailState::Durable;
    let later_lsn = retired.append(later, stamp());
    for receipt in retired.receipts.clone() {
        retired.append(
            Record::MailState {
                id: receipt.id,
                state: MailState::Acked,
            },
            stamp(),
        );
    }
    let reader = retired.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    let row = state(&projected, retired.agent);
    assert!(row.preparation.is_none());
    assert_eq!(row.covered_frontier, retired.receipts[1].lsn);
    assert_eq!(
        row.candidate.as_ref().unwrap().trigger,
        MailRef { id, lsn: later_lsn }
    );
    assert_eq!(projected.agent(retired.agent).unwrap().unwrap().turns, 0);
}

#[test]
fn canonical_goal_changes_retire_unissued_authority_and_refuse_late_resume() {
    for change in 0..4 {
        let mut log = Log::new(&[0]);
        let root = log.append(log.root_record(), stamp());
        let record = match change {
            0 => Record::GoalCleared {
                agent: log.agent,
                goal: log.goal,
            },
            1 => {
                let mut value = samples::record(RecordKind::GoalSet);
                let Record::GoalSet { goal, .. } = &mut value else {
                    unreachable!();
                };
                goal.id = GoalId::from_parts(1, 123);
                value
            }
            2 => Record::GoalReassigned {
                goal: log.goal,
                from: log.agent,
                to: AgentId::from_parts(1, 90),
                reason: None,
            },
            _ => Record::AgentRetired {
                id: log.agent,
                reason: "owned fixture".into(),
                browser_retire: None,
            },
        };
        log.append(record, stamp());
        let reader = Reader::new(&log.journal);
        let mut projected = Projections::in_memory().unwrap();
        replay(&reader, &mut projected).unwrap();
        assert!(state(&projected, log.agent).preparation.is_none());
        assert_eq!(
            state(&projected, log.agent).covered_frontier,
            log.receipts[0].lsn
        );
        let stale = log.append(log.resume_record(root), stamp());
        log.writer.take();
        assert!(
            matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == stale)
        );
    }
}

#[test]
fn final_issue_stamp_is_exact_and_not_resampled() {
    for late_issue in [false, true] {
        let mut log = Log::new(&[0]);
        let root = log.append(log.root_record(), stamp());
        log.advance(0);
        let decision = log.append(log.resume_record(root), stamp());
        let at = if late_issue {
            stamp().saturating_add_millis(1)
        } else {
            stamp()
        };
        let issue = log.append(log.issue_record(root, decision), at);
        log.writer.take();
        if late_issue {
            assert!(
                matches!(Reader::open(&log.journal), Err(Error::MailWake { lsn, .. }) if lsn == issue)
            );
        } else {
            let reader = Reader::open(&log.journal).unwrap().0;
            let mut projected = Projections::in_memory().unwrap();
            replay(&reader, &mut projected).unwrap();
            assert_eq!(projected.agent(log.agent).unwrap().unwrap().turns, 1);
        }
    }
}

fn quota_refusal(log: &Log, digest: u8) -> Record {
    let mut record = log.root_record();
    let Record::MailWakeTurnDecided(decision) = &mut record else {
        unreachable!();
    };
    decision.purpose = MailWakePurpose::UnpreparedCandidate {
        trigger: log.receipts[0],
        frontier: log.receipts[0].lsn,
    };
    decision.input_digest.value = Hex32::new([digest; 32]);
    decision.gate_trace.truncate(5);
    decision.gate_trace.last_mut().unwrap().fired = true;
    decision.decision = MailWakeAdmission::Exhausted {
        reason: dudes_proto::ExhaustedReason::MailWakeLimit,
    };
    record
}

#[test]
fn issue_day_buckets_admit_twenty_preserve_refusal_and_survive_backward_clock() {
    let mut log = Log::new(&[0]);
    for index in 0..20_u128 {
        if index != 0 {
            log.new_receipt(index, stamp());
        }
        let root = log.append(log.root_record(), stamp());
        log.advance(0);
        let decision = log.append(log.resume_record(root), stamp());
        let mut record = log.issue_record(root, decision);
        let turn = TurnId::from_parts(1, index + 1_000);
        let Record::MailWakeOnlyTurnIssued(issue) = &mut record else {
            unreachable!();
        };
        issue.turn = turn;
        log.append(record, stamp());
        log.finish_issue(turn, stamp());
    }
    log.new_receipt(20, stamp());
    log.append(quota_refusal(&log, 3), stamp());
    let next_day = stamp().saturating_add_millis(86_400_000);
    let root = log.append(log.root_record(), next_day);
    log.append(
        Record::MailState {
            id: log.receipts[0].id,
            state: MailState::Notified { n: 1 },
        },
        next_day,
    );
    let decision = log.append(log.resume_record(root), next_day);
    let mut record = log.issue_record(root, decision);
    let turn = TurnId::from_parts(1, 1_020);
    let Record::MailWakeOnlyTurnIssued(issue) = &mut record else {
        unreachable!();
    };
    issue.turn = turn;
    log.append(record, next_day);
    log.finish_issue(turn, next_day);
    log.new_receipt(21, next_day);
    let owed_root = log.append(log.root_record(), next_day);
    log.append(
        Record::MailState {
            id: log.receipts[0].id,
            state: MailState::Notified { n: 1 },
        },
        next_day,
    );
    let mut retained = quota_refusal(&log, 4);
    let Record::MailWakeTurnDecided(decision) = &mut retained else {
        unreachable!();
    };
    decision.purpose = MailWakePurpose::ExistingPreparation {
        root_lsn: owed_root,
    };
    decision.input_digest.domain = MailWakeDigestDomain::Resume;
    log.append(retained, stamp());
    let reader = log.reader();
    let mut projected = Projections::in_memory().unwrap();
    replay(&reader, &mut projected).unwrap();
    assert_eq!(projected.agent(log.agent).unwrap().unwrap().turns, 21);
    assert_eq!(projected.goal(log.goal).unwrap().unwrap().turns_on_goal, 21);
    assert_eq!(
        projected.goal(log.goal).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    let row = state(&projected, log.agent);
    assert_eq!(row.preparation.as_ref().unwrap().root_lsn, owed_root);
    assert_eq!(
        row.preparation.as_ref().unwrap().plan.trigger,
        log.receipts[0]
    );
    assert!(row.candidate.is_none());
    assert!(matches!(
        row.latest.unwrap().decision,
        MailWakeAdmission::Exhausted {
            reason: dudes_proto::ExhaustedReason::MailWakeLimit
        }
    ));
    let export = projected.export().unwrap();
    let mut counts: Vec<u8> = export
        .lines()
        .filter_map(|line| line.strip_prefix("mail_wake_by_day "))
        .map(|line| serde_json::from_str(line.split_once(' ').unwrap().1).unwrap())
        .collect();
    counts.sort_unstable();
    assert_eq!(counts, [1, 20]);
    assert_eq!(replay(&reader, &mut projected).unwrap().applied, 0);
    assert_eq!(projected.export().unwrap(), export);
}
