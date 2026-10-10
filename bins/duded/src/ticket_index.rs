//! What replay and the live writer need to know about tickets to refuse a forged start.
//!
//! A `TicketStarted` carries a whole goal. The daemon builds that goal from the ticket and the
//! agent, so a record whose goal does not match them was not written by the daemon, and nothing
//! may install it. The index is built from the ticket records as they are validated, in journal
//! order, and holds only what that check reads.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use dudes_proto::rpc::goal::CompletionCheckDeclaration;
use dudes_proto::{CheckBaseline, CompletionCheckRef, Record, TicketCheck};
use dudes_supervisor::world::World;
use dudes_types::{AgentId, CardId, CardStatus};

use crate::goal_replacement::named_check;

/// The name a ticket's goal records for its check. Every ticket goal starts as a check that may
/// already pass, so the name is always framed under that domain.
pub(crate) fn goal_check(check: &TicketCheck) -> Option<CompletionCheckRef> {
    let declaration = CompletionCheckDeclaration::new(&check.id, &check.label).ok()?;
    named_check(&declaration, &check.predicate, true)
}

/// The card a record that only a ticket can receive names. A start has its own, stricter check.
const fn ticket_target(record: &Record) -> Option<&CardId> {
    match record {
        Record::TicketEdited { card, .. }
        | Record::TicketAssigned { card, .. }
        | Record::TicketRemoved { card, .. } => Some(card),
        _ => None,
    }
}

#[derive(Debug)]
struct Ticket {
    project: String,
    check: TicketCheck,
    status: CardStatus,
    holder: Option<AgentId>,
    removed: bool,
}

/// The tickets the journal has made so far, and the project each agent was hired into.
#[derive(Debug, Default)]
pub(crate) struct TicketIndex {
    hires: BTreeMap<AgentId, Option<String>>,
    tickets: BTreeMap<CardId, Ticket>,
}

impl TicketIndex {
    /// Refuse a `TicketStarted` that does not match the ticket it starts.
    pub(crate) fn validate(&self, world: &World, record: &Record) -> Result<()> {
        if let Some(card) = ticket_target(record) {
            ensure!(
                self.tickets.get(card).is_some_and(|ticket| !ticket.removed),
                "a ticket record names a card that is not a live ticket"
            );
        }
        let Record::TicketStarted {
            card, agent, goal, ..
        } = record
        else {
            return Ok(());
        };
        let Some(ticket) = self.tickets.get(card).filter(|ticket| !ticket.removed) else {
            bail!("a ticket start names a ticket that is absent or removed");
        };
        ensure!(
            ticket.status == CardStatus::Claimed && ticket.holder == Some(*agent),
            "a ticket start names an agent that does not hold the ticket in its queue"
        );
        ensure!(
            self.hires.get(agent) == Some(&Some(ticket.project.clone())),
            "a ticket start names an agent that was not hired into the ticket's project"
        );
        ensure!(
            goal.predicate == ticket.check.predicate,
            "a ticket start carries a predicate other than the ticket's check"
        );
        ensure!(
            goal.baseline == Some(CheckBaseline::MayAlreadyPass),
            "a ticket start carries a baseline other than may-already-pass"
        );
        let expected = goal_check(&ticket.check);
        ensure!(
            expected.is_some() && goal.completion_check == expected,
            "a ticket start carries a check name that does not match the ticket's check"
        );
        ensure!(
            !world.goals.contains_key(&goal.id),
            "a ticket start reuses a goal id"
        );
        Ok(())
    }

    /// Fold a record the writer or replay accepted.
    pub(crate) fn apply(&mut self, record: &Record) {
        match record {
            Record::AgentHired { id, workspace, .. } => {
                self.hires.insert(
                    *id,
                    workspace.as_ref().map(|workspace| workspace.id.clone()),
                );
            }
            Record::TicketCreated {
                card,
                project,
                done_when,
                ..
            } => {
                self.tickets.insert(
                    *card,
                    Ticket {
                        project: project.clone(),
                        check: done_when.clone(),
                        status: CardStatus::Todo,
                        holder: None,
                        removed: false,
                    },
                );
            }
            Record::TicketEdited {
                card, done_when, ..
            } => self.with(*card, |ticket| ticket.check = done_when.clone()),
            Record::TicketAssigned { card, agent, .. } | Record::Claim { card, agent, .. } => {
                self.with(*card, |ticket| {
                    ticket.status = CardStatus::Claimed;
                    ticket.holder = Some(*agent);
                });
            }
            Record::TicketStarted { card, agent, .. } => self.with(*card, |ticket| {
                ticket.status = CardStatus::Doing;
                ticket.holder = Some(*agent);
            }),
            Record::CardTransition { card, to, .. } => self.with(*card, |ticket| {
                ticket.status = *to;
                if *to == CardStatus::Todo {
                    ticket.holder = None;
                }
            }),
            Record::TicketRemoved { card, .. } => self.with(*card, |ticket| ticket.removed = true),
            Record::AgentRetired { id, .. } => {
                for ticket in self.tickets.values_mut() {
                    if ticket.holder == Some(*id) && ticket.status != CardStatus::Review {
                        ticket.holder = None;
                        if ticket.status != CardStatus::Done {
                            ticket.status = CardStatus::Todo;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn with(&mut self, card: CardId, change: impl FnOnce(&mut Ticket)) {
        if let Some(ticket) = self.tickets.get_mut(&card) {
            change(ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::record::{RecordedWorkspace, samples};
    use dudes_proto::{Actor, Entry, Goal, RecordKind};
    use dudes_types::{GoalId, Lsn, Seq, Timestamp};

    use super::*;

    fn agent() -> AgentId {
        AgentId::from_parts(1, 1)
    }

    fn card() -> CardId {
        CardId::from_parts(4, 1)
    }

    fn check() -> TicketCheck {
        TicketCheck {
            id: "default".into(),
            label: "tests pass".into(),
            predicate: dudes_proto::Predicate::Command {
                argv: vec!["/usr/bin/true".into()],
                timeout_s: 30,
            },
        }
    }

    fn hire(project: Option<&str>) -> Record {
        let mut hired = samples::record(RecordKind::AgentHired);
        let Record::AgentHired { id, workspace, .. } = &mut hired else {
            unreachable!("the sample is a hire");
        };
        *id = agent();
        *workspace = project.map(|id| RecordedWorkspace {
            id: id.into(),
            label: "Service".into(),
        });
        hired
    }

    fn created() -> Record {
        Record::TicketCreated {
            card: card(),
            title: "Fix".into(),
            description: String::new(),
            department: "engineering".into(),
            project: "service".into(),
            done_when: check(),
            by: Actor::Peer { pid: 1, uid: None },
        }
    }

    fn assigned() -> Record {
        Record::TicketAssigned {
            card: card(),
            agent: agent(),
            by: Actor::Peer { pid: 1, uid: None },
            message: None,
            expected_seq: Seq(0),
        }
    }

    /// The goal the daemon builds for the ticket's check.
    fn honest_goal() -> Goal {
        let check = check();
        let declaration = CompletionCheckDeclaration::new(&check.id, &check.label).unwrap();
        Goal {
            id: GoalId::from_parts(8, 1),
            text: "Fix".into(),
            completion_check: named_check(&declaration, &check.predicate, true),
            predicate: check.predicate,
            baseline: Some(CheckBaseline::MayAlreadyPass),
            budget: dudes_proto::GoalBudget {
                usd: None,
                tokens: None,
            },
            hop_cap: 4,
        }
    }

    fn start(goal: Goal) -> Record {
        Record::TicketStarted {
            card: card(),
            agent: agent(),
            goal,
            expected_seq: Seq(1),
        }
    }

    /// An index that has seen the ticket made and given to the agent, hired into `project`.
    fn queued(project: Option<&str>) -> TicketIndex {
        let mut index = TicketIndex::default();
        for record in [hire(project), created(), assigned()] {
            index.apply(&record);
        }
        index
    }

    fn refusal(index: &TicketIndex, world: &World, record: &Record) -> String {
        format!("{:#}", index.validate(world, record).unwrap_err())
    }

    #[test]
    fn ticket_start_is_validated_by_the_ticket_index() {
        let world = World::default();
        let index = queued(Some("service"));
        index
            .validate(&world, &start(honest_goal()))
            .expect("the goal the daemon builds is admitted");
        assert!(
            index.validate(&world, &created()).is_ok(),
            "other records pass"
        );
    }

    #[test]
    fn a_forged_ticket_start_is_refused() {
        let world = World::default();
        let index = queued(Some("service"));
        let honest = honest_goal();

        let mut other_command = honest.clone();
        other_command.predicate = dudes_proto::Predicate::Command {
            argv: vec!["/usr/bin/false".into()],
            timeout_s: 30,
        };
        assert!(refusal(&index, &world, &start(other_command)).contains("predicate"));

        for baseline in [
            None,
            Some(CheckBaseline::BoardNotMet),
            Some(CheckBaseline::CommandFailed {
                program: "/usr/bin/true".into(),
                exit: 1,
            }),
        ] {
            let mut forged = honest.clone();
            forged.baseline = baseline;
            assert!(refusal(&index, &world, &start(forged)).contains("baseline"));
        }

        let mut renamed = honest.clone();
        renamed.completion_check.as_mut().unwrap().label = "Forged label".into();
        assert!(refusal(&index, &world, &start(renamed)).contains("check name"));
        let mut unnamed = honest.clone();
        unnamed.completion_check = None;
        assert!(refusal(&index, &world, &start(unnamed)).contains("check name"));
        let mut wrong_domain = honest.clone();
        let declaration = CompletionCheckDeclaration::new("default", "tests pass").unwrap();
        wrong_domain.completion_check = named_check(&declaration, &check().predicate, false);
        assert!(refusal(&index, &world, &start(wrong_domain)).contains("check name"));

        let mut reused = World::default();
        reused.apply(
            Lsn(1),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::GoalSet {
                    agent: agent(),
                    goal: honest.clone(),
                    browser_onboarding: None,
                    browser_goal: None,
                    goal_replacement: None,
                },
            },
        );
        assert!(refusal(&index, &reused, &start(honest)).contains("goal id"));
    }

    #[test]
    fn a_ticket_start_names_a_live_queued_ticket_of_the_agents_own_project() {
        let world = World::default();
        let goal = honest_goal();

        let mut unknown = TicketIndex::default();
        unknown.apply(&hire(Some("service")));
        assert!(refusal(&unknown, &world, &start(goal.clone())).contains("absent"));

        let mut removed = queued(Some("service"));
        removed.apply(&Record::TicketRemoved {
            card: card(),
            by: Actor::System,
            expected_seq: Seq(1),
        });
        assert!(refusal(&removed, &world, &start(goal.clone())).contains("absent or removed"));

        let mut unqueued = TicketIndex::default();
        for record in [hire(Some("service")), created()] {
            unqueued.apply(&record);
        }
        assert!(refusal(&unqueued, &world, &start(goal.clone())).contains("queue"));

        let mut other = queued(Some("service"));
        other.apply(&Record::CardTransition {
            card: card(),
            from: CardStatus::Claimed,
            to: CardStatus::Todo,
            by: Actor::System,
            expected_seq: Seq(1),
            reason: None,
        });
        assert!(refusal(&other, &world, &start(goal.clone())).contains("queue"));

        let elsewhere = queued(Some("another"));
        assert!(refusal(&elsewhere, &world, &start(goal.clone())).contains("project"));
        let unrecorded = queued(None);
        assert!(refusal(&unrecorded, &world, &start(goal.clone())).contains("project"));

        let mut retired = queued(Some("service"));
        retired.apply(&Record::AgentRetired {
            id: agent(),
            reason: "left".into(),
            browser_retire: None,
        });
        assert!(refusal(&retired, &world, &start(goal.clone())).contains("queue"));

        let mut started = queued(Some("service"));
        started.apply(&start(goal.clone()));
        assert!(
            refusal(&started, &world, &start(goal)).contains("queue"),
            "a ticket that is already working cannot start again"
        );
    }

    /// A ticket edited after it was queued is started with the edited check, and a start built
    /// from the check it had before is refused.
    #[test]
    fn the_start_must_carry_the_tickets_current_check() {
        let world = World::default();
        let mut index = queued(Some("service"));
        let edited = TicketCheck {
            id: "lint".into(),
            label: "lint is clean".into(),
            predicate: dudes_proto::Predicate::Command {
                argv: vec!["/usr/bin/lint".into()],
                timeout_s: 30,
            },
        };
        index.apply(&Record::TicketEdited {
            card: card(),
            title: "Fix".into(),
            description: String::new(),
            done_when: edited.clone(),
            by: Actor::System,
            expected_seq: Seq(1),
        });
        assert!(index.validate(&world, &start(honest_goal())).is_err());
        let declaration = CompletionCheckDeclaration::new(&edited.id, &edited.label).unwrap();
        let goal = Goal {
            completion_check: named_check(&declaration, &edited.predicate, true),
            predicate: edited.predicate,
            ..honest_goal()
        };
        index
            .validate(&world, &start(goal))
            .expect("the edited check is current");
    }

    /// An edit, an assignment or a removal names a live ticket, and a plain card is not one.
    #[test]
    fn a_ticket_record_for_a_plain_card_or_a_removed_ticket_is_refused() {
        let world = World::default();
        let plain = CardId::from_parts(4, 9);
        let mut index = queued(Some("service"));
        index.apply(&Record::CardCreated {
            card: plain,
            title: "Plain".into(),
            department: "engineering".into(),
            by: Actor::System,
            deps: Vec::new(),
        });
        let records = |card| {
            [
                Record::TicketEdited {
                    card,
                    title: "x".into(),
                    description: String::new(),
                    done_when: check(),
                    by: Actor::System,
                    expected_seq: Seq(0),
                },
                Record::TicketAssigned {
                    card,
                    agent: agent(),
                    by: Actor::System,
                    message: None,
                    expected_seq: Seq(0),
                },
                Record::TicketRemoved {
                    card,
                    by: Actor::System,
                    expected_seq: Seq(0),
                },
            ]
        };
        for record in records(plain) {
            assert!(refusal(&index, &world, &record).contains("live ticket"));
        }
        for record in records(card()) {
            index.validate(&world, &record).expect("a live ticket");
        }
        index.apply(&Record::TicketRemoved {
            card: card(),
            by: Actor::System,
            expected_seq: Seq(1),
        });
        for record in records(card()) {
            assert!(refusal(&index, &world, &record).contains("live ticket"));
        }
    }

    /// The pass that validates every goal change runs the ticket check on a start, and a start
    /// is no replacement: it leaves both replacement indexes empty.
    #[test]
    fn goal_index_ignores_a_ticket_start() {
        let world = World::default();
        let mut changes = crate::goal_replacement::GoalChanges::default();
        for record in [hire(Some("service")), created(), assigned()] {
            changes.validate(&world, &record).unwrap();
            changes.apply(&record);
        }
        let mut forged = honest_goal();
        forged.baseline = None;
        assert!(changes.validate(&world, &start(forged)).is_err());
        let honest = start(honest_goal());
        changes.validate(&world, &honest).unwrap();
        changes.apply(&honest);
        let anything = dudes_proto::record::Hex32::new([0; 32]);
        assert!(changes.get(anything).is_none());
        assert!(changes.operator(anything).is_none());
    }
}
