#![allow(missing_docs)]

use dudes_policy::{
    BrowserNoteInput, BudgetState, Ceilings, CompletedNoteInput, CompletedNoteState, Control,
};
use dudes_proto::{Entry, Gate, GateStep, NoteAdmission, NoteContract, Record, Spend};
use dudes_supervisor::World;
use dudes_types::{AgentId, GoalId, Lsn, Timestamp, TurnId};
use serde_json::json;

fn agent() -> AgentId {
    AgentId::from_parts(1, 1)
}
fn goal() -> GoalId {
    GoalId::from_parts(1, 2)
}
fn turn() -> TurnId {
    TurnId::from_parts(1, 3)
}
fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    }
}
fn seed() -> World {
    let mut world = World::default();
    for (i,value) in [
        json!({"t":"agent_hired","id":agent(),"name":"Owned","role":"test","department":"test","provider":"claude","cwd":"/work","budget":{}}),
        json!({"t":"goal_set","agent":agent(),"goal":{"id":goal(),"text":"closed","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":1}}),
        json!({"t":"loop_decided","agent":agent(),"input_digest":"original","gate_trace":[],"decision":{"kind":"done"}}),
        json!({"t":"browser_note_queued","agent":agent(),"text":"whole note","destination":{"kind":"managed_goal","goal":goal()},"browser":{"request_id_digest":"11".repeat(32),"request_digest":"22".repeat(32),"session_digest":"33".repeat(32)}}),
    ].into_iter().enumerate() {world.apply(Lsn(u64::try_from(i).unwrap()+1),&entry(serde_json::from_value(value).unwrap()));}
    world
}
fn line() -> dudes_proto::IdentityLine {
    dudes_proto::IdentityLine::new(agent(), "Owned", "test")
}
fn task(contract: NoteContract) -> String {
    match contract {
        NoteContract::Raw => "whole note".into(),
        NoteContract::Taught => dudes_proto::taught_note_task("whole note").unwrap(),
        NoteContract::IdentifiedTaught => {
            dudes_proto::identified_taught_note_task(&line(), "whole note").unwrap()
        }
    }
}
fn decision(contract: NoteContract) -> Record {
    let task = task(contract);
    let decision = NoteAdmission::Issue { task };
    match contract {
        NoteContract::Raw => Record::NoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "raw".into(),
            gate_trace: vec![],
            decision,
        },
        NoteContract::Taught => Record::TaughtNoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "taught".into(),
            gate_trace: Gate::ORDER
                .into_iter()
                .map(|gate| GateStep {
                    gate,
                    fired: gate == Gate::Work,
                    note: None,
                })
                .collect(),
            decision,
        },
        NoteContract::IdentifiedTaught => Record::IdentifiedTaughtNoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "identified".into(),
            gate_trace: Gate::ORDER
                .into_iter()
                .map(|gate| GateStep {
                    gate,
                    fired: gate == Gate::Work,
                    note: None,
                })
                .collect(),
            decision,
        },
    }
}
fn issue(contract: NoteContract) -> Record {
    let name = match contract {
        NoteContract::Raw => "note_only_turn_issued",
        NoteContract::Taught => "taught_note_only_turn_issued",
        NoteContract::IdentifiedTaught => "identified_taught_note_only_turn_issued",
    };
    serde_json::from_value(json!({"t":name,"agent":agent(),"turn":turn(),"goal":goal(),"task":task(contract),"session":null,"mode":"managed","model_selection":{},"lsn_of_decision":5,"answers":[],"note_lsn":4,"completion_lsn":3})).unwrap()
}
#[test]
fn world_matches_every_note_authority_without_task_rewrites_or_second_charges() {
    const ALL: [NoteContract; 3] = [
        NoteContract::Raw,
        NoteContract::Taught,
        NoteContract::IdentifiedTaught,
    ];
    for deciding in ALL {
        for issuing in ALL {
            let mut world = seed();
            world.apply(Lsn(5), &entry(decision(deciding)));
            world.apply(Lsn(6), &entry(issue(issuing)));
            assert_eq!(world.completed_goal_lsn(goal()), Some(Lsn(3)));
            if deciding == issuing {
                assert_eq!(world.turns[&turn()].task, task(issuing));
                assert_eq!(world.browser_issue(turn()).unwrap().contract, Some(issuing));
                world.apply(Lsn(7), &entry(issue(issuing)));
                assert_eq!(world.goals[&goal()].turns_on_goal, 1);
            } else {
                assert!(!world.turns.contains_key(&turn()));
                assert_eq!(world.goals[&goal()].turns_on_goal, 0);
            }
        }
    }
}
#[test]
fn completed_taught_issue_saturates_without_reopening_the_original_completion() {
    let mut world = seed();
    world.goals.get_mut(&goal()).unwrap().turns_on_goal = u32::MAX;
    world.apply(Lsn(5), &entry(decision(NoteContract::Taught)));
    world.apply(Lsn(6), &entry(issue(NoteContract::Taught)));
    assert_eq!(world.goals[&goal()].turns_on_goal, u32::MAX);
    assert_eq!(world.completed_goal_lsn(goal()), Some(Lsn(3)));
    world.apply(Lsn(7), &entry(issue(NoteContract::Taught)));
    assert_eq!(world.goals[&goal()].turns_on_goal, u32::MAX);
}
#[test]
fn identified_digest_is_distinct_and_covers_the_same_full_input_including_the_line() {
    let mut input = CompletedNoteInput::new(
        goal(),
        Lsn(3),
        BrowserNoteInput::new(Lsn(4), goal(), "whole note".into()).unwrap(),
        line(),
        CompletedNoteState {
            control: Control::default(),
            last_turn_end: None,
            open_question: None,
            budget_state: BudgetState::default(),
            ceilings: Ceilings {
                goal: dudes_proto::GoalBudget {
                    usd: None,
                    tokens: None,
                },
                agent: dudes_proto::AgentBudget {
                    usd_per_day: None,
                    tokens_per_day: None,
                },
                floor_usd_per_day: None,
            },
            per_turn_floor: Spend::DEFAULT_PER_TURN_FLOOR,
            rung: dudes_types::BreakerRung::Healthy,
            turns_on_goal: 1,
            hop_cap: 1,
        },
    )
    .unwrap();
    let before = serde_json::to_vec(&input).unwrap();
    let raw = dudes_supervisor::host::completed_note_input_digest(&input).unwrap();
    let taught = dudes_supervisor::host::identified_note_input_digest(&input).unwrap();
    assert_ne!(raw, taught);
    assert_eq!(taught.len(), 64);
    assert_eq!(serde_json::to_vec(&input).unwrap(), before);
    input.state.turns_on_goal = u32::MAX;
    assert_ne!(
        dudes_supervisor::host::identified_note_input_digest(&input).unwrap(),
        taught
    );
}
