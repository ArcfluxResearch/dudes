use std::collections::{BTreeMap, BTreeSet};

use dudes_journal::{JournalOptions, Projections, Writer};
use dudes_proto::record::samples;
use dudes_proto::{
    Actor, CardView, Entry, Evidence, InstructionAcceptCompletion, InstructionBaselineRef,
    InstructionComparison, InstructionEndBoundary, InstructionObservationOwner,
    InstructionReviewAction, InstructionSettlement, InstructionTicketAssociation, LoopDecision,
    Predicate, Record, RecordKind, TicketRow, TicketWait,
};
use dudes_supervisor::world::World;
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Lsn, Timestamp, TurnId};

use super::Facts;

const PROJECT: &str = "service";
const OTHER_PROJECT: &str = "other";

struct ReviewFixture {
    _directory: tempfile::TempDir,
    writer: Writer,
    projections: Projections,
    world: World,
    hires: BTreeMap<AgentId, String>,
    owner: InstructionObservationOwner,
    ticket: InstructionTicketAssociation,
    started_lsn: Lsn,
    end_lsn: Lsn,
}

impl ReviewFixture {
    fn ended() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = dudes_journal::open(directory.path(), JournalOptions::default()).unwrap();
        let Record::InstructionObservationStarted { owner, .. } =
            samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!();
        };
        let Record::TicketStarted { card, goal, .. } = samples::record(RecordKind::TicketStarted)
        else {
            unreachable!();
        };
        let mut fixture = Self {
            _directory: directory,
            writer,
            projections: Projections::in_memory().unwrap(),
            world: World::default(),
            hires: BTreeMap::new(),
            owner,
            ticket: InstructionTicketAssociation {
                card,
                goal: goal.id,
                assignment_lsn: Lsn(0),
                seq_at_start: dudes_types::Seq::ZERO,
            },
            started_lsn: Lsn(0),
            end_lsn: Lsn(0),
        };
        fixture.hire(fixture.agent(), PROJECT);
        fixture.create_and_assign(card, fixture.agent(), PROJECT);
        let mut started = samples::record(RecordKind::TicketStarted);
        if let Record::TicketStarted { expected_seq, .. } = &mut started {
            *expected_seq = fixture.row(card).0.seq;
        }
        fixture.append(started);
        fixture.ticket = fixture
            .world
            .instructions
            .association_for_goal(goal.id, fixture.row(card).0.seq)
            .unwrap();
        let decision = fixture.append(samples::record(RecordKind::LoopDecided));
        let mut observation = samples::record(RecordKind::InstructionObservationStarted);
        if let Record::InstructionObservationStarted { ticket, .. } = &mut observation {
            *ticket = Some(fixture.ticket);
        }
        fixture.started_lsn = fixture.append(observation);
        let mut issue = samples::record(RecordKind::TurnIssued);
        if let Record::TurnIssued {
            lsn_of_decision,
            task,
            session,
            answers,
            ..
        } = &mut issue
        {
            *lsn_of_decision = decision;
            *task = "next".into();
            *session = None;
            answers.clear();
        }
        fixture.append(issue);
        let mut spawn = samples::record(RecordKind::TurnSpawned);
        if let Record::TurnSpawned {
            session, reissued, ..
        } = &mut spawn
        {
            *session = None;
            *reissued = false;
        }
        fixture.append(spawn);
        let mut ended = samples::record(RecordKind::TurnEnded);
        if let Record::TurnEnded { session, .. } = &mut ended {
            *session = None;
        }
        fixture.end_lsn = fixture.append(ended);
        fixture
    }

    fn completed() -> Self {
        let mut fixture = Self::ended();
        let Record::InstructionObservationStarted { snapshot, .. } =
            samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!();
        };
        fixture.append(Record::InstructionObservationFinished {
            owner: fixture.owner,
            ticket: Some(fixture.ticket),
            baseline: InstructionBaselineRef::Started {
                lsn: fixture.started_lsn,
            },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::ManagedTurn {
                    turn: fixture.turn(),
                    end_lsn: fixture.end_lsn,
                },
                sampled_at: Timestamp::from_unix_millis(1),
                recovery: false,
                comparison: InstructionComparison {
                    changes: Vec::new(),
                    gaps: snapshot.gaps,
                    usage: snapshot.usage,
                },
            },
        });
        let Predicate::Command { argv, .. } = &fixture.world.goals[&fixture.goal()].goal.predicate
        else {
            unreachable!();
        };
        let mut verdict = samples::record(RecordKind::PredicateResult);
        if let Record::PredicateResult { evidence, .. } = &mut verdict {
            *evidence = Evidence::Command {
                argv: argv.clone(),
                program: Some("/toolchain/bin/cargo".into()),
                exit: Some(0),
                output_tail: "ok".into(),
            };
        }
        fixture.append(verdict);
        let mut done = samples::record(RecordKind::LoopDecided);
        if let Record::LoopDecided { decision, .. } = &mut done {
            *decision = LoopDecision::Done;
        }
        let done_lsn = fixture.append(done);
        assert_eq!(
            fixture.world.completed_goal_lsn(fixture.goal()),
            Some(done_lsn)
        );
        assert_eq!(
            fixture
                .projections
                .goal(fixture.goal())
                .unwrap()
                .unwrap()
                .first_done
                .unwrap()
                .lsn,
            done_lsn
        );
        assert_eq!(fixture.row(fixture.card()).0.status, CardStatus::Blocked);
        assert!(fixture.world.instructions.pending_for_goal(fixture.goal()));
        fixture
    }

    fn append(&mut self, record: Record) -> Lsn {
        let entry = Entry {
            at: Timestamp::from_unix_millis(1),
            record,
        };
        self.world.validate_record_authority(&entry).unwrap();
        self.projections.validate_next_instruction(&entry).unwrap();
        let (lsn, crc) = self.writer.append_witnessed(&entry).unwrap();
        assert!(self.projections.apply(lsn, crc, &entry).unwrap());
        self.world.apply(lsn, &entry);
        assert_eq!(self.world.last_applied_lsn, Some(lsn));
        lsn
    }

    fn agent(&self) -> AgentId {
        let InstructionObservationOwner::Managed { agent, .. } = self.owner else {
            unreachable!();
        };
        agent
    }

    fn turn(&self) -> TurnId {
        let InstructionObservationOwner::Managed { turn, .. } = self.owner else {
            unreachable!();
        };
        turn
    }

    fn goal(&self) -> GoalId {
        self.ticket.goal
    }

    fn card(&self) -> CardId {
        self.ticket.card
    }

    fn row(&self, card: CardId) -> (CardView, TicketRow) {
        self.projections.ticket(card).unwrap().unwrap()
    }

    fn hire(&mut self, agent: AgentId, project: &str) {
        let mut hired = samples::record(RecordKind::AgentHired);
        if let Record::AgentHired { id, .. } = &mut hired {
            *id = agent;
        }
        self.append(hired);
        self.hires.insert(agent, project.into());
    }

    fn create_and_assign(&mut self, card: CardId, agent: AgentId, project: &str) {
        let mut created = samples::record(RecordKind::TicketCreated);
        if let Record::TicketCreated {
            card: created_card,
            project: created_project,
            ..
        } = &mut created
        {
            *created_card = card;
            *created_project = project.into();
        }
        self.append(created);
        self.append(Record::TicketAssigned {
            card,
            agent,
            by: Actor::Peer { pid: 77, uid: None },
            message: None,
            expected_seq: self.row(card).0.seq,
        });
    }

    fn read<T>(&self, read: impl FnOnce(&Facts<'_>) -> T) -> T {
        let tickets = self.projections.tickets().unwrap();
        read(&Facts {
            world: &self.world,
            tickets: &tickets,
            hires: &self.hires,
            working: &BTreeSet::new(),
            closing: false,
        })
    }

    fn review_record(&self, action: InstructionReviewAction) -> Record {
        let (view, ticket) = self.row(self.card());
        Record::TicketInstructionReviewed {
            card: self.card(),
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action,
        }
    }
}

#[test]
fn completed_instruction_review_keeps_project_reserved() {
    let mut fixture = ReviewFixture::completed();
    let second = AgentId::from_parts(1, 2);
    let waiting = CardId::from_parts(4, 2);
    fixture.hire(second, PROJECT);
    fixture.create_and_assign(waiting, second, PROJECT);
    fixture.read(|facts| {
        assert_eq!(facts.agent_wait(second, PROJECT), None);
        assert_eq!(
            facts.project_wait(PROJECT),
            Some(TicketWait::Ticket {
                card: fixture.card()
            })
        );
        assert_eq!(facts.next_ticket(PROJECT), None);
        assert!(facts.owed_ticket_moves().is_empty());
        let (view, ticket) = fixture.row(fixture.card());
        assert!(!facts.holds_unfinished_goal(&view, &ticket));
        assert_eq!(facts.goal_to_settle(&view, &ticket), None);
    });
}

#[test]
fn continuing_acknowledgment_preserves_only_the_recovery_owned_wait() {
    for recovery in [false, true] {
        let mut fixture = ReviewFixture::ended();
        let Record::InstructionObservationStarted { snapshot, .. } =
            samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!();
        };
        fixture.append(Record::InstructionObservationFinished {
            owner: fixture.owner,
            ticket: Some(fixture.ticket),
            baseline: InstructionBaselineRef::Started {
                lsn: fixture.started_lsn,
            },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::ManagedTurn {
                    turn: fixture.turn(),
                    end_lsn: fixture.end_lsn,
                },
                sampled_at: Timestamp::from_unix_millis(1),
                recovery: false,
                comparison: InstructionComparison {
                    changes: Vec::new(),
                    gaps: snapshot.gaps,
                    usage: snapshot.usage,
                },
            },
        });
        if recovery {
            let card = fixture.row(fixture.card()).0;
            fixture.append(Record::CardTransition {
                card: card.id,
                from: card.status,
                to: CardStatus::Blocked,
                by: Actor::System,
                expected_seq: card.seq,
                reason: Some(dudes_proto::TicketReason::RecoveryRequired),
            });
        }
        let before = fixture.row(fixture.card());
        let record = fixture.review_record(InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Continue,
        });
        fixture.append(record);
        let after = fixture.row(fixture.card());
        assert_eq!(
            after.0.status,
            if recovery {
                CardStatus::Blocked
            } else {
                CardStatus::Doing
            }
        );
        assert_eq!(
            after.1.reason,
            recovery.then_some(dudes_proto::TicketReason::RecoveryRequired)
        );
        assert_eq!(
            (after.0.claimed_by, after.1.goal, after.1.round),
            (before.0.claimed_by, before.1.goal, before.1.round)
        );
        assert!(!fixture.world.instructions.pending_for_goal(fixture.goal()));
    }
}

#[test]
fn completed_instruction_review_keeps_owner_reserved() {
    let fixture = ReviewFixture::completed();
    fixture.read(|facts| {
        let (view, ticket) = fixture.row(fixture.card());
        assert!(!facts.holds_unfinished_goal(&view, &ticket));
        assert_eq!(facts.goal_to_settle(&view, &ticket), None);
        assert_eq!(
            facts.agent_wait(fixture.agent(), PROJECT),
            Some(TicketWait::Ticket {
                card: fixture.card()
            })
        );
    });
}

#[test]
fn another_project_and_owner_can_start_while_instruction_review_waits() {
    let mut fixture = ReviewFixture::completed();
    let other = AgentId::from_parts(1, 3);
    let waiting = CardId::from_parts(4, 3);
    fixture.hire(other, OTHER_PROJECT);
    fixture.create_and_assign(waiting, other, OTHER_PROJECT);
    fixture.read(|facts| {
        assert_eq!(facts.agent_wait(other, OTHER_PROJECT), None);
        assert_eq!(facts.project_wait(OTHER_PROJECT), None);
        assert_eq!(facts.next_ticket(OTHER_PROJECT), Some(waiting));
    });
}

#[test]
fn exact_accept_releases_completed_review_project_and_owner_reservations() {
    let mut fixture = ReviewFixture::completed();
    let waiting = CardId::from_parts(4, 2);
    fixture.create_and_assign(waiting, fixture.agent(), PROJECT);
    let review = fixture.review_record(InstructionReviewAction::Accept {
        completion: InstructionAcceptCompletion::Completed {
            completion_lsn: fixture.world.completed_goal_lsn(fixture.goal()).unwrap(),
        },
    });
    fixture.append(review.clone());
    assert_eq!(fixture.row(fixture.card()).0.status, CardStatus::Done);
    assert!(!fixture.world.instructions.pending_for_goal(fixture.goal()));
    fixture.read(|facts| {
        assert_eq!(facts.agent_wait(fixture.agent(), PROJECT), None);
        assert_eq!(facts.project_wait(PROJECT), None);
        assert_eq!(facts.next_ticket(PROJECT), Some(waiting));
    });
    assert!(
        fixture
            .projections
            .validate_next_instruction(&Entry {
                at: Timestamp::from_unix_millis(1),
                record: review,
            })
            .is_err()
    );
}

#[test]
fn exact_changes_requested_releases_review_and_keeps_rework_first() {
    let mut fixture = ReviewFixture::completed();
    let waiting = CardId::from_parts(4, 2);
    fixture.create_and_assign(waiting, fixture.agent(), PROJECT);
    fixture.append(
        fixture.review_record(InstructionReviewAction::ChangesRequested {
            agent: fixture.agent(),
            message: Some("check the shared instruction".into()),
            clear_goal: None,
        }),
    );
    assert_eq!(fixture.row(fixture.card()).0.status, CardStatus::Claimed);
    assert!(!fixture.world.instructions.pending_for_goal(fixture.goal()));
    assert!(!fixture.world.goals[&fixture.goal()].cleared);
    fixture.read(|facts| {
        assert_eq!(facts.agent_wait(fixture.agent(), PROJECT), None);
        assert_eq!(facts.project_wait(PROJECT), None);
        assert_eq!(facts.next_ticket(PROJECT), Some(fixture.card()));
        assert_eq!(
            facts.queue(PROJECT).get(1).map(|(card, _, _)| *card),
            Some(waiting)
        );
    });
}

#[test]
fn outstanding_ending_comparison_keeps_reservations_and_refuses_accept() {
    let fixture = ReviewFixture::ended();
    assert!(
        fixture
            .world
            .instructions
            .observation_pending(fixture.card())
    );
    fixture.read(|facts| {
        assert_eq!(
            facts.agent_wait(fixture.agent(), PROJECT),
            Some(TicketWait::AgentBusy)
        );
        assert_eq!(
            facts.project_wait(PROJECT),
            Some(TicketWait::Ticket {
                card: fixture.card()
            })
        );
    });
    let record = fixture.review_record(InstructionReviewAction::Accept {
        completion: InstructionAcceptCompletion::Continue,
    });
    assert!(
        fixture
            .projections
            .validate_next_instruction(&Entry {
                at: Timestamp::from_unix_millis(1),
                record,
            })
            .is_err()
    );
    assert!(fixture.world.instructions.pending_for_goal(fixture.goal()));
}
