use dudes_journal::{Error, Fsync, JournalOptions, Projections, Writer};
use dudes_proto::diff_stamp::TicketBinding;
use dudes_proto::record::samples;
use dudes_proto::recovery::{RecoveryBinding, RecoveryWork};
use dudes_proto::recovery_state::{RecoveryFacts, RecoveryStateError};
use dudes_proto::{Actor, Entry, Evidence, Goal, LoopDecision, Predicate, Record, RecordKind};
use dudes_supervisor::World;
use dudes_types::{
    AgentId, CardId, CardStatus, GoalId, Lsn, Mode, ModelSelection, Provenance, Provider, Seq,
    Timestamp, TurnId, WorktreeId,
};

fn agent() -> AgentId {
    AgentId::from_parts(51, 1)
}
fn goal() -> GoalId {
    GoalId::from_parts(51, 2)
}
fn turn() -> TurnId {
    TurnId::from_parts(51, 3)
}
fn card() -> CardId {
    CardId::from_parts(51, 4)
}
fn peer() -> Actor {
    Actor::Peer {
        pid: 77,
        uid: Some(501),
    }
}

fn definition(predicate: Predicate) -> Goal {
    let Record::GoalSet { mut goal, .. } = samples::record(RecordKind::GoalSet) else {
        unreachable!()
    };
    goal.id = self::goal();
    goal.predicate = predicate;
    goal
}

fn command() -> Predicate {
    Predicate::Command {
        argv: vec!["check".into()],
        timeout_s: 1,
    }
}

struct Prefix {
    dir: tempfile::TempDir,
    writer: Writer,
    persistent: Projections,
    world: World,
}

impl Prefix {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (writer, _) = dudes_journal::open(
            &dir.path().join("journal"),
            JournalOptions {
                fsync: Fsync::None,
                checkpoint_every: 0,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        Self {
            dir,
            writer,
            persistent: Projections::in_memory().unwrap(),
            world: World::default(),
        }
    }

    fn append(&mut self, record: Record) -> Lsn {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        };
        let (lsn, crc) = self.writer.append_witnessed(&entry).unwrap();
        self.persistent.apply(lsn, crc, &entry).unwrap();
        self.world.apply(lsn, &entry);
        assert_eq!(self.world.last_applied_lsn, Some(lsn));
        lsn
    }

    fn hired(&mut self, id: AgentId, worktree: Option<WorktreeId>) -> Lsn {
        let mut record = samples::record(RecordKind::AgentHired);
        let Record::AgentHired {
            id: actual,
            worktree: lease,
            provider,
            ..
        } = &mut record
        else {
            unreachable!()
        };
        *actual = id;
        *lease = worktree;
        *provider = Provider::Codex;
        self.append(record)
    }

    fn issue(&mut self, answers: Vec<Lsn>) -> Lsn {
        let mut decision = samples::record(RecordKind::LoopDecided);
        let Record::LoopDecided {
            agent: owner,
            decision: action,
            ..
        } = &mut decision
        else {
            unreachable!()
        };
        *owner = agent();
        *action = LoopDecision::Continue {
            task: "original immutable task".into(),
        };
        let at = self.append(decision);
        self.append(Record::TurnIssued {
            agent: agent(),
            turn: turn(),
            goal: goal(),
            mode: Mode::Managed,
            lsn_of_decision: at,
            task: "original immutable task".into(),
            session: None,
            model_selection: Some(ModelSelection::provider_default()),
            answers,
        })
    }

    fn facts(&mut self, binding: &RecoveryBinding) -> Result<RecoveryFacts, RecoveryStateError> {
        let before = self.persistent.export().unwrap();
        let bytes = journal_bytes(&self.dir.path().join("journal"));
        let persistent = self
            .writer
            .checked_recovery(&mut self.persistent)
            .unwrap()
            .facts(binding)
            .map_err(|error| match error {
                Error::RecoveryState(reason) => reason,
                other => panic!("unexpected factual refusal: {other}"),
            });
        assert_eq!(persistent, self.world.recovery_facts(binding));
        assert_eq!(self.persistent.export().unwrap(), before);
        assert_eq!(journal_bytes(&self.dir.path().join("journal")), bytes);
        persistent
    }

    fn assert_replay(&mut self, binding: &RecoveryBinding) {
        let expected = self.facts(binding);
        let mut rebuilt = Projections::in_memory().unwrap();
        dudes_journal::replay(&self.writer.reader(), &mut rebuilt).unwrap();
        let mut world = World::default();
        for entry in self.writer.reader().iter_from(Lsn::ZERO).unwrap() {
            let (lsn, entry) = entry.unwrap();
            world.apply(lsn, &entry);
        }
        assert_eq!(self.persistent.export().unwrap(), rebuilt.export().unwrap());
        let persistent = self
            .writer
            .checked_recovery(&mut rebuilt)
            .unwrap()
            .facts(binding)
            .map_err(|error| match error {
                Error::RecoveryState(reason) => reason,
                other => panic!("unexpected replay factual refusal: {other}"),
            });
        assert_eq!(persistent, expected);
        assert_eq!(world.recovery_facts(binding), expected);
    }
}

fn journal_bytes(path: &std::path::Path) -> Vec<(u32, Vec<u8>)> {
    dudes_journal::segment::list_segments(path)
        .unwrap()
        .into_iter()
        .map(|id| {
            (
                id,
                std::fs::read(dudes_journal::segment::segment_path(path, id)).unwrap(),
            )
        })
        .collect()
}

fn ordinary(predicate: Predicate) -> (Prefix, RecoveryBinding) {
    let mut prefix = Prefix::new();
    let hired = prefix.hired(agent(), None);
    if let Predicate::BoardState { card, .. } = predicate {
        let mut created = samples::record(RecordKind::TicketCreated);
        let Record::TicketCreated { card: id, .. } = &mut created else {
            unreachable!()
        };
        *id = card;
        prefix.append(created);
    }
    prefix.append(Record::GoalSet {
        agent: agent(),
        goal: definition(predicate),
        browser_onboarding: None,
        browser_goal: None,
        goal_replacement: None,
    });
    prefix.append(Record::HumanQuestion {
        agent: agent(),
        text: "Which input?".into(),
    });
    let answer = prefix.append(Record::HumanAnswer {
        agent: agent(),
        text: "The original input.".into(),
        provenance: None,
        browser_answer: None,
    });
    let issue = prefix.issue(vec![answer]);
    (
        prefix,
        RecoveryBinding {
            work: RecoveryWork {
                agent: agent(),
                goal: goal(),
                turn: turn(),
            },
            issue_lsn: issue,
            hired_lsn: hired,
            ticket: None,
            worktree: None,
            provider: Provider::Codex,
            model_selection: Some(ModelSelection::provider_default()),
        },
    )
}

fn ticket_prefix(worktree: Option<WorktreeId>) -> (Prefix, RecoveryBinding) {
    let mut prefix = Prefix::new();
    if let Some(id) = worktree {
        let mut leased = samples::record(RecordKind::WorktreeLeased);
        let Record::WorktreeLeased { worktree, .. } = &mut leased else {
            unreachable!()
        };
        *worktree = id;
        prefix.append(leased);
    }
    let hired = prefix.hired(agent(), worktree);
    let mut created = samples::record(RecordKind::TicketCreated);
    let Record::TicketCreated { card: id, .. } = &mut created else {
        unreachable!()
    };
    *id = card();
    prefix.append(created);
    prefix.append(Record::TicketAssigned {
        card: card(),
        agent: agent(),
        by: peer(),
        message: None,
        expected_seq: Seq::ZERO,
    });
    prefix.append(Record::TicketStarted {
        card: card(),
        agent: agent(),
        goal: definition(command()),
        expected_seq: Seq(1),
    });
    (
        prefix,
        RecoveryBinding {
            work: RecoveryWork {
                agent: agent(),
                goal: goal(),
                turn: turn(),
            },
            issue_lsn: Lsn::ZERO,
            hired_lsn: hired,
            ticket: Some(TicketBinding {
                card: card(),
                round: 1,
                goal: goal(),
                claimant: agent(),
            }),
            worktree,
            provider: Provider::Codex,
            model_selection: Some(ModelSelection::provider_default()),
        },
    )
}

#[test]
fn one_actual_prefix_has_equal_inert_facts_and_exact_binding_refusals() {
    let (mut prefix, binding) = ordinary(command());
    let facts = prefix.facts(&binding).unwrap();
    assert_eq!(facts.binding, binding);
    assert_eq!(facts.source_issue.issued.answers.len(), 1);
    assert_eq!(facts.source_issue.issued.work, binding.work);
    for changed in 0..9 {
        let mut target = binding.clone();
        match changed {
            0 => target.hired_lsn = target.issue_lsn,
            1 => target.issue_lsn = target.hired_lsn,
            2 => target.work.agent = AgentId::from_parts(51, 99),
            3 => target.work.goal = GoalId::from_parts(51, 99),
            4 => target.work.turn = TurnId::from_parts(51, 99),
            5 => target.provider = Provider::Claude,
            6 => target.model_selection = None,
            7 => target.worktree = Some(WorktreeId::from_bytes([0x99; 16])),
            8 => {
                target.ticket = Some(TicketBinding {
                    card: card(),
                    round: 1,
                    goal: goal(),
                    claimant: agent(),
                })
            }
            _ => unreachable!(),
        }
        assert_eq!(
            prefix.facts(&target).err(),
            Some(RecoveryStateError::Binding),
            "case {changed}"
        );
    }
    prefix.assert_replay(&binding);
}

#[test]
fn changed_world_task_answers_and_control_do_not_replace_immutable_sources() {
    let (mut prefix, binding) = ordinary(command());
    for changed in 0..3 {
        let mut world = prefix.world.clone();
        let expected = match changed {
            0 => {
                world.turns.get_mut(&turn()).unwrap().task.push('!');
                RecoveryStateError::Binding
            }
            1 => {
                world.turns.get_mut(&turn()).unwrap().answers.clear();
                RecoveryStateError::Binding
            }
            2 => {
                world.agents.get_mut(&agent()).unwrap().control_lsn = Some(binding.issue_lsn);
                RecoveryStateError::Control
            }
            _ => unreachable!(),
        };
        assert_eq!(world.recovery_facts(&binding).err(), Some(expected));
    }
    prefix.facts(&binding).unwrap();
}

#[test]
fn pause_and_resume_witnesses_keep_the_actual_before_record_control() {
    let (mut prefix, binding) = ordinary(command());
    let paused = prefix.append(Record::Pause {
        agent: agent(),
        browser: None,
    });
    assert_eq!(prefix.facts(&binding).unwrap().control_lsn, Some(paused));
    let resumed = prefix.append(Record::Resume {
        agent: agent(),
        browser: None,
    });
    assert_eq!(prefix.facts(&binding).unwrap().control_lsn, Some(resumed));
    let source = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .witness(resumed)
        .unwrap();
    assert_eq!(&source, prefix.world.recovery_witness(resumed).unwrap());
    assert_eq!(source.context().previous_control.unwrap().lsn, Some(paused));
    prefix.assert_replay(&binding);
}

fn predicate_pass(prefix: &mut Prefix, by: Actor) {
    let mut record = samples::record(RecordKind::PredicateResult);
    let Record::PredicateResult {
        goal: target,
        turn: source,
        pass,
        provenance,
        evidence,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *target = goal();
    *source = Some(turn());
    *pass = true;
    *provenance = Provenance::Observed;
    *evidence = Evidence::Card {
        card: card(),
        status: CardStatus::Done,
        by,
    };
    prefix.append(record);
}

fn transition(prefix: &mut Prefix, status: CardStatus, by: Actor) {
    let seq = prefix
        .persistent
        .cards()
        .unwrap()
        .into_iter()
        .find(|row| row.id == card())
        .unwrap()
        .seq;
    let mut record = samples::record(RecordKind::CardTransition);
    let Record::CardTransition {
        card: target,
        to,
        by: author,
        expected_seq,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *target = card();
    *to = status;
    *author = by;
    *expected_seq = seq;
    prefix.append(record);
}

#[test]
fn qualified_board_evidence_and_exact_invalidation_guards_agree() {
    let (mut prefix, binding) = ordinary(Predicate::BoardState {
        card: card(),
        status: CardStatus::Done,
    });
    let sibling = AgentId::from_parts(51, 9);
    transition(
        &mut prefix,
        CardStatus::Done,
        Actor::Agent { agent: sibling },
    );
    predicate_pass(&mut prefix, Actor::Agent { agent: sibling });
    prefix.facts(&binding).unwrap();
    transition(&mut prefix, CardStatus::Done, peer());
    predicate_pass(&mut prefix, peer());
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.append(Record::CardCreated {
        card: CardId::from_parts(51, 99),
        title: "unrelated".into(),
        department: "engineering".into(),
        by: peer(),
        deps: Vec::new(),
    });
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    transition(&mut prefix, CardStatus::Review, peer());
    prefix.facts(&binding).unwrap();
    assert!(
        prefix
            .persistent
            .goal(goal())
            .unwrap()
            .unwrap()
            .last_result
            .unwrap()
            .pass
    );
    assert!(prefix.world.goals[&goal()].predicate.is_none());
    transition(&mut prefix, CardStatus::Done, peer());
    predicate_pass(&mut prefix, peer());
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.hired(sibling, None);
    let mut retired = samples::record(RecordKind::AgentRetired);
    let Record::AgentRetired { id, .. } = &mut retired else {
        unreachable!()
    };
    *id = sibling;
    prefix.append(retired);
    prefix.facts(&binding).unwrap();
    predicate_pass(&mut prefix, peer());
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.assert_replay(&binding);
}

#[test]
fn current_ticket_round_and_original_lease_cannot_be_redirected() {
    let lease = WorktreeId::from_bytes([0x51; 16]);
    let (mut prefix, mut binding) = ticket_prefix(Some(lease));
    binding.issue_lsn = prefix.issue(Vec::new());
    prefix.facts(&binding).unwrap();
    let mut wrong = binding.clone();
    wrong.ticket.as_mut().unwrap().round += 1;
    assert_eq!(
        prefix.facts(&wrong).err(),
        Some(RecoveryStateError::Binding)
    );
    let mut retired = samples::record(RecordKind::AgentRetired);
    let Record::AgentRetired { id, .. } = &mut retired else {
        unreachable!()
    };
    *id = agent();
    prefix.append(retired);
    prefix.append(Record::WorktreeRemoved {
        worktree: lease,
        how: dudes_proto::WorktreeRemoval::Operator,
    });
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.assert_replay(&binding);
}

#[test]
fn all_card_mutation_source_families_have_actual_kind_and_owner() {
    let (mut prefix, binding) = ordinary(command());
    let created = prefix.append(Record::CardCreated {
        card: card(),
        title: "source".into(),
        department: "engineering".into(),
        by: peer(),
        deps: Vec::new(),
    });
    let claimed = prefix.append(Record::Claim {
        card: card(),
        agent: agent(),
        expected_seq: Seq::ZERO,
    });
    let mut moved = samples::record(RecordKind::CardTransition);
    let Record::CardTransition {
        card: id,
        expected_seq,
        ..
    } = &mut moved
    else {
        unreachable!()
    };
    *id = card();
    *expected_seq = Seq(1);
    let moved = prefix.append(moved);
    let other = CardId::from_parts(51, 98);
    let mut ticket = samples::record(RecordKind::TicketCreated);
    let Record::TicketCreated { card: id, .. } = &mut ticket else {
        unreachable!()
    };
    *id = other;
    let ticket = prefix.append(ticket);
    for (at, kind, id) in [
        (created, RecordKind::CardCreated, card()),
        (claimed, RecordKind::Claim, card()),
        (moved, RecordKind::CardTransition, card()),
        (ticket, RecordKind::TicketCreated, other),
    ] {
        let source = prefix
            .writer
            .checked_recovery(&mut prefix.persistent)
            .unwrap()
            .witness(at)
            .unwrap();
        assert_eq!(&source, prefix.world.recovery_witness(at).unwrap());
        assert_eq!(source.reference().kind, kind);
        assert_eq!(
            source.reference().owner,
            dudes_proto::recovery_state::history::RecoveryWitnessOwner::Card(id)
        );
    }
    prefix.facts(&binding).unwrap();
}

#[test]
fn completed_cleared_and_missing_card_targets_remain_unavailable() {
    let (mut prefix, binding) = ordinary(Predicate::BoardState {
        card: card(),
        status: CardStatus::Done,
    });
    transition(&mut prefix, CardStatus::Done, peer());
    predicate_pass(&mut prefix, peer());
    let mut done = samples::record(RecordKind::LoopDecided);
    let Record::LoopDecided {
        agent: owner,
        decision,
        ..
    } = &mut done
    else {
        unreachable!()
    };
    *owner = agent();
    *decision = LoopDecision::Done;
    prefix.append(done);
    transition(&mut prefix, CardStatus::Review, peer());
    assert!(prefix.world.goals[&goal()].predicate.as_ref().unwrap().pass);
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.append(Record::GoalCleared {
        agent: agent(),
        goal: goal(),
    });
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );

    let (mut prefix, binding) = ordinary(Predicate::BoardState {
        card: card(),
        status: CardStatus::Done,
    });
    let seq = prefix.persistent.ticket(card()).unwrap().unwrap().0.seq;
    let mut removed = samples::record(RecordKind::TicketRemoved);
    let Record::TicketRemoved {
        card: id,
        expected_seq,
        ..
    } = &mut removed
    else {
        unreachable!()
    };
    *id = card();
    *expected_seq = seq;
    prefix.append(removed);
    assert!(prefix.persistent.ticket(card()).unwrap().is_none());
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.assert_replay(&binding);
}

fn instruction_review_prefix() -> (Prefix, RecoveryBinding) {
    use dudes_proto::instruction::{
        InstructionBaselineRef, InstructionComparison, InstructionCoverageGap, InstructionGapKind,
        InstructionGapOrigin, InstructionIssueAssociation, InstructionObservationOwner,
        InstructionPathContext, InstructionSampleUsage, InstructionSettlement,
        InstructionUnknownReason,
    };
    let (mut prefix, mut binding) = ticket_prefix(None);
    let association = prefix
        .world
        .instructions
        .recorded_association_for_goal(goal())
        .unwrap();
    let owner = InstructionObservationOwner::Managed {
        agent: agent(),
        turn: turn(),
        issue: InstructionIssueAssociation::Prospective,
    };
    let mut started = samples::record(RecordKind::InstructionObservationStarted);
    let Record::InstructionObservationStarted {
        owner: actual,
        ticket,
        ..
    } = &mut started
    else {
        unreachable!()
    };
    *actual = owner;
    *ticket = Some(association);
    let began = prefix.append(started);
    binding.issue_lsn = prefix.issue(Vec::new());
    let mut ended = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent: actual,
        turn: actual_turn,
        ..
    } = &mut ended
    else {
        unreachable!()
    };
    *actual = agent();
    *actual_turn = turn();
    prefix.append(ended);
    prefix.append(Record::InstructionObservationFinished {
        owner,
        ticket: Some(association),
        baseline: InstructionBaselineRef::Started { lsn: began },
        settlement: InstructionSettlement::Unknown {
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
                    agent: None,
                    path: None,
                }],
                usage: InstructionSampleUsage::default(),
            },
        },
    });
    (prefix, binding)
}

#[test]
fn only_the_validated_instruction_outcome_supplies_a_derived_goal_clear() {
    use dudes_proto::instruction::{InstructionGoalClear, InstructionReviewAction};
    let (mut prefix, binding) = instruction_review_prefix();
    prefix.facts(&binding).unwrap();
    let (view, ticket) = prefix.persistent.ticket(card()).unwrap().unwrap();
    let clear = InstructionGoalClear {
        agent: agent(),
        goal: goal(),
    };
    let reviewed = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TicketInstructionReviewed {
            card: card(),
            by: peer(),
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: InstructionReviewAction::ChangesRequested {
                agent: agent(),
                message: Some("rework".into()),
                clear_goal: Some(clear),
            },
        },
    };
    let mut refused = reviewed.clone();
    let Record::TicketInstructionReviewed {
        action: InstructionReviewAction::ChangesRequested { clear_goal, .. },
        ..
    } = &mut refused.record
    else {
        unreachable!()
    };
    *clear_goal = None;
    let before = prefix.persistent.export().unwrap();
    let bytes = journal_bytes(&prefix.dir.path().join("journal"));
    assert!(matches!(
        prefix
            .writer
            .checked_recovery(&mut prefix.persistent)
            .unwrap()
            .append(&refused),
        Err(Error::InstructionAdmission { .. })
    ));
    assert_eq!(prefix.persistent.export().unwrap(), before);
    assert_eq!(journal_bytes(&prefix.dir.path().join("journal")), bytes);
    let actual = prefix.append(reviewed.record);
    let source = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .witness(actual)
        .unwrap();
    assert_eq!(&source, prefix.world.recovery_witness(actual).unwrap());
    assert_eq!(source.context().cleared_goal, Some(clear));
    assert_eq!(prefix.world.agents[&agent()].goal, None);
    assert_eq!(
        prefix.persistent.agent(agent()).unwrap().unwrap().goal,
        None
    );
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    prefix.assert_replay(&binding);
}

#[test]
fn a_confirmed_session_keeps_its_actual_source_across_a_later_issue() {
    let (mut prefix, binding) = ordinary(command());
    let session = dudes_types::SessionId::new("confirmed-earlier-turn");
    let mut spawn = samples::record(RecordKind::TurnSpawned);
    let Record::TurnSpawned {
        agent: owner,
        turn: actual,
        session: named,
        ..
    } = &mut spawn
    else {
        unreachable!()
    };
    *owner = agent();
    *actual = turn();
    *named = Some(session.clone());
    prefix.append(spawn);
    let confirmed = prefix.append(Record::SessionLearned {
        agent: agent(),
        turn: turn(),
        session: session.clone(),
    });
    let mut end = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent: owner,
        turn: actual,
        session: named,
        ..
    } = &mut end
    else {
        unreachable!()
    };
    *owner = agent();
    *actual = turn();
    *named = Some(session.clone());
    prefix.append(end);
    let second = TurnId::from_parts(51, 33);
    let mut decision = samples::record(RecordKind::LoopDecided);
    let Record::LoopDecided {
        agent: owner,
        decision: action,
        ..
    } = &mut decision
    else {
        unreachable!()
    };
    *owner = agent();
    *action = LoopDecision::Continue {
        task: "next immutable task".into(),
    };
    let decision = prefix.append(decision);
    let issued = prefix.append(Record::TurnIssued {
        agent: agent(),
        turn: second,
        goal: goal(),
        mode: Mode::Managed,
        lsn_of_decision: decision,
        task: "next immutable task".into(),
        session: None,
        model_selection: Some(ModelSelection::provider_default()),
        answers: Vec::new(),
    });
    let mut target = binding;
    target.work.turn = second;
    target.issue_lsn = issued;
    assert_eq!(
        prefix.facts(&target).unwrap().confirmed_session,
        Some(session)
    );
    assert_eq!(prefix.world.agents[&agent()].session_lsn, Some(confirmed));
    assert_eq!(
        prefix
            .persistent
            .agent(agent())
            .unwrap()
            .unwrap()
            .session_lsn,
        Some(confirmed)
    );
    prefix.assert_replay(&target);
}

#[test]
fn clearing_an_owed_goal_keeps_the_original_issue_task_answers_and_hop() {
    let (mut prefix, binding) = ordinary(command());
    let before = prefix.world.turns[&turn()].clone();
    assert_eq!(before.status, dudes_types::TurnStatus::Issued);
    assert_eq!(prefix.world.goals[&goal()].turns_on_goal, 1);
    prefix.append(Record::GoalCleared {
        agent: agent(),
        goal: goal(),
    });
    assert_eq!(
        prefix.facts(&binding).err(),
        Some(RecoveryStateError::Binding)
    );
    let after = &prefix.world.turns[&turn()];
    assert_eq!(after.issue_lsn, binding.issue_lsn);
    assert_eq!(after.status, dudes_types::TurnStatus::Issued);
    assert_eq!(after.task, before.task);
    assert_eq!(after.answers, before.answers);
    assert_eq!(after.model_selection, before.model_selection);
    assert_eq!(prefix.world.goals[&goal()].turns_on_goal, 1);
    let turn = prefix.persistent.turn(turn()).unwrap().unwrap();
    assert_eq!(turn.lsn_issued, binding.issue_lsn);
    assert_eq!(turn.status, dudes_types::TurnStatus::Issued);
    assert_eq!(
        prefix
            .persistent
            .goal(goal())
            .unwrap()
            .unwrap()
            .turns_on_goal,
        1
    );
    prefix.assert_replay(&binding);
}
