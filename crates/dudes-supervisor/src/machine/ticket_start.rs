//! A ticket's start installs a goal, so every fold that reacts to a goal being installed must
//! react to it exactly as it does to a `GoalSet` for the same agent and goal. Each test feeds the
//! two carriers to one site and compares the outcomes.

use dudes_proto::{AutomaticHold, Entry, Goal, LiveActivityKind, Predicate, Record};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Lsn, Seq, Timestamp, TurnId};

use super::Supervisor;
use crate::Input;
use crate::testing::{FakeClock, FakeRng, Harness, journal_with_issued_turn};

/// How the new goal reaches the agent.
#[derive(Clone, Copy, Debug)]
enum Carrier {
    GoalSet,
    TicketStart,
}

const BOTH: [Carrier; 2] = [Carrier::GoalSet, Carrier::TicketStart];

struct Fixture {
    harness: Harness,
    agent: AgentId,
    old: GoalId,
    replacement: Goal,
}

impl Fixture {
    /// An agent that holds one goal and is replayed up to, not including, its first turn.
    fn new() -> Self {
        let (entries, agent, _) = journal_with_issued_turn(None, "old goal");
        let mut harness = Harness::new();
        for (entry, position) in entries[..2].iter().zip(1u64..) {
            harness.step(Input::Replayed {
                lsn: Lsn(position),
                entry: entry.clone(),
            });
        }
        let Record::GoalSet { goal, .. } = &entries[1].record else {
            unreachable!("the fixture's second entry sets the goal");
        };
        Self {
            harness,
            agent,
            old: goal.id,
            replacement: Goal {
                id: GoalId::from_parts(77, 1),
                text: "the ticket".to_owned(),
                ..goal.clone()
            },
        }
    }

    fn record(&self, carrier: Carrier) -> Record {
        match carrier {
            Carrier::GoalSet => Record::GoalSet {
                agent: self.agent,
                goal: self.replacement.clone(),
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
            Carrier::TicketStart => Record::TicketStarted {
                card: CardId::from_parts(9, 9),
                agent: self.agent,
                goal: self.replacement.clone(),
                expected_seq: Seq(1),
            },
        }
    }

    fn replay(&mut self, carrier: Carrier) {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: self.record(carrier),
        };
        self.harness.step(Input::Replayed { lsn: Lsn(3), entry });
    }

    fn supervisor(&mut self) -> &mut Supervisor<FakeClock, FakeRng> {
        &mut self.harness.supervisor
    }
}

fn each(mut check: impl FnMut(Carrier, &mut Fixture)) {
    for carrier in BOTH {
        let mut fixture = Fixture::new();
        check(carrier, &mut fixture);
    }
}

#[test]
fn world_installs_a_ticket_goal() {
    let mut outcomes = Vec::new();
    each(|carrier, fixture| {
        let agent = fixture.agent;
        fixture
            .supervisor()
            .world
            .agents
            .get_mut(&agent)
            .unwrap()
            .automatic_hold = Some(AutomaticHold::LaunchRefused {
            reason: "stale".into(),
        });
        fixture.replay(carrier);
        let (new, old) = (fixture.replacement.id, fixture.old);
        let world = &fixture.supervisor().world;
        let row = &world.agents[&agent];
        assert_eq!(row.goal, Some(new), "{carrier:?}");
        assert_eq!(
            row.automatic_hold, None,
            "{carrier:?} lifts an automatic hold"
        );
        assert_eq!(world.goals[&new].agent, agent, "{carrier:?}");
        assert!(!world.goals[&new].cleared);
        assert!(world.goals.contains_key(&old), "the old row stays");
        outcomes.push(format!(
            "{:?} {:?}",
            world.agents[&agent], world.goals[&new]
        ));
    });
    assert_eq!(outcomes[0], outcomes[1]);
}

#[test]
fn machine_resets_activity_on_ticket_start() {
    each(|carrier, fixture| {
        let agent = fixture.agent;
        fixture
            .supervisor()
            .activity_pending
            .insert(agent, (None::<TurnId>, LiveActivityKind::UsingTool));
        fixture.replay(carrier);
        assert!(
            !fixture.supervisor().activity_pending.contains_key(&agent),
            "{carrier:?} drops the agent's pending activity"
        );
    });
}

#[test]
fn machine_rechecks_board_goals_on_ticket_start() {
    each(|carrier, fixture| {
        let (agent, old) = (fixture.agent, fixture.old);
        let board = Predicate::BoardState {
            card: CardId::from_parts(5, 5),
            status: CardStatus::Done,
        };
        fixture
            .supervisor()
            .world
            .goals
            .get_mut(&old)
            .unwrap()
            .goal
            .predicate = board;
        fixture.supervisor().goals_awaiting_board_result.insert(old);
        assert_eq!(fixture.supervisor().world.agents[&agent].goal, Some(old));
        fixture.replay(carrier);
        assert!(
            fixture.supervisor().goals_awaiting_board_result.is_empty(),
            "{carrier:?} stops waiting on the replaced goal's board result"
        );
    });
}

#[test]
fn ticket_start_refreshes_the_owner_mail_wake() {
    each(|carrier, fixture| {
        let record = fixture.record(carrier);
        assert_eq!(
            fixture.supervisor().mail_event_recipients(&record),
            vec![fixture.agent],
            "{carrier:?}"
        );
    });
}

#[test]
fn ticket_start_affects_its_agent() {
    each(|carrier, fixture| {
        let record = fixture.record(carrier);
        assert_eq!(
            fixture.supervisor().world.mail_wake_recipients(&record),
            vec![fixture.agent],
            "{carrier:?}"
        );
    });
}
