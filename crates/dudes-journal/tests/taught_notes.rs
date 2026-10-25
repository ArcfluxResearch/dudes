#![allow(missing_docs)]

use dudes_journal::{Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::{Entry, Gate, GateStep, NoteAdmission, NoteContract, Record};
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
fn seed() -> Vec<Entry> {
    vec![
        serde_json::from_value(json!({"at":0,"t":"agent_hired","id":agent(),"name":"Owned","role":"test","department":"test","provider":"claude","cwd":"/work","budget":{}})).unwrap(),
        serde_json::from_value(json!({"at":0,"t":"goal_set","agent":agent(),"goal":{"id":goal(),"text":"completed goal stays closed","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":1}})).unwrap(),
        entry(Record::LoopDecided { agent:agent(),input_digest:"original".into(),gate_trace:vec![],decision:dudes_proto::LoopDecision::Done }),
        serde_json::from_value(json!({"at":0,"t":"browser_note_queued","agent":agent(),"text":"whole note","destination":{"kind":"managed_goal","goal":goal()},"browser":{"request_id_digest":"11".repeat(32),"request_digest":"22".repeat(32),"session_digest":"33".repeat(32)}})).unwrap(),
    ]
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
    let value = NoteAdmission::Issue { task };
    match contract {
        NoteContract::Raw => Record::NoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "raw".into(),
            gate_trace: vec![],
            decision: value,
        },
        NoteContract::Taught => Record::TaughtNoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "taught".into(),
            gate_trace: trace(),
            decision: value,
        },
        NoteContract::IdentifiedTaught => Record::IdentifiedTaughtNoteTurnDecided {
            agent: agent(),
            goal: goal(),
            note_lsn: Lsn(4),
            completion_lsn: Lsn(3),
            input_digest: "identified".into(),
            gate_trace: trace(),
            decision: value,
        },
    }
}
fn issue(contract: NoteContract) -> Record {
    let name = match contract {
        NoteContract::Raw => "note_only_turn_issued",
        NoteContract::Taught => "taught_note_only_turn_issued",
        NoteContract::IdentifiedTaught => "identified_taught_note_only_turn_issued",
    };
    serde_json::from_value(json!({"t":name,
        "agent":agent(),"turn":turn(),"goal":goal(),"task":task(contract),
        "session":null,"mode":"managed","model_selection":{},"lsn_of_decision":5,"answers":[],"note_lsn":4,"completion_lsn":3})).unwrap()
}
fn projected(history: &[Entry]) -> Projections {
    let mut p = Projections::in_memory().unwrap();
    for (i, e) in history.iter().enumerate() {
        p.apply(Lsn(u64::try_from(i).unwrap() + 1), 0, e).unwrap();
    }
    p
}

#[test]
fn raw_taught_and_identified_decision_issue_contracts_must_match() {
    const ALL: [NoteContract; 3] = [
        NoteContract::Raw,
        NoteContract::Taught,
        NoteContract::IdentifiedTaught,
    ];
    for decision_contract in ALL {
        for issue_contract in ALL {
            let mut history = seed();
            history.push(entry(decision(decision_contract)));
            let mut p = projected(&history);
            let before = p.export().unwrap();
            let result = p.apply(Lsn(6), 0, &entry(issue(issue_contract)));
            if decision_contract == issue_contract {
                result.unwrap();
                assert_eq!(
                    p.goal(goal()).unwrap().unwrap().status,
                    dudes_types::GoalStatus::Achieved
                );
                assert_eq!(p.goal(goal()).unwrap().unwrap().turns_on_goal, 1);
                assert_eq!(p.chat().unwrap().turns[0].text, "whole note");
                assert!(p.export().unwrap().contains(match decision_contract {
                    NoteContract::Raw => "\"raw\"",
                    NoteContract::Taught => "\"taught\"",
                    NoteContract::IdentifiedTaught => "\"identified_taught\"",
                }));
                let before = p.export().unwrap();
                assert!(p.apply(Lsn(7), 0, &entry(issue(issue_contract))).is_err());
                assert_eq!(p.export().unwrap(), before);
            } else {
                assert!(result.is_err());
                assert_eq!(p.export().unwrap(), before);
                assert_eq!(p.last_applied_lsn(), Lsn(5));
            }
        }
    }
}

#[test]
fn new_decision_verdict_trace_mismatch_rolls_back_without_replacing_raw_authority() {
    let mut history = seed();
    history.push(entry(decision(NoteContract::Raw)));
    let mut p = projected(&history);
    let before = p.export().unwrap();
    let mut invalid = decision(NoteContract::Taught);
    if let Record::TaughtNoteTurnDecided { decision, .. } = &mut invalid {
        *decision = NoteAdmission::Wait {
            question: "not a Work verdict".into(),
        };
    }
    assert!(p.apply(Lsn(6), 0, &entry(invalid)).is_err());
    assert_eq!(p.export().unwrap(), before);
    p.apply(Lsn(6), 0, &entry(issue(NoteContract::Raw)))
        .unwrap();
}

#[test]
fn taught_activity_retains_only_existing_finite_turn_facts() {
    let mut history = seed();
    history.push(entry(decision(NoteContract::Taught)));
    history.push(entry(issue(NoteContract::Taught)));
    let p = projected(&history);
    let activity = p.activity().unwrap();
    let new = activity
        .iter()
        .filter(|row| row.lsn.parse::<u64>().unwrap() >= 5)
        .collect::<Vec<_>>();
    assert_eq!(new.len(), 1);
    assert!(matches!(
        new[0].event,
        dudes_proto::ActivityEvent::TurnIssued { .. }
    ));
    let bytes = serde_json::to_string(&new).unwrap();
    for forbidden in [
        "whole note",
        "completed goal stays closed",
        "completion_lsn",
        "input_digest",
        "answers",
        "task",
    ] {
        assert!(!bytes.contains(forbidden));
    }
}

fn append_joined_note_history(
    journal: &std::path::Path,
    issue_contract: NoteContract,
) -> (Lsn, Lsn) {
    let mut writer = open(
        journal,
        JournalOptions {
            fsync: Fsync::None,
            ..JournalOptions::default()
        },
    )
    .unwrap()
    .0;
    let history = seed();
    for record in &history[..2] {
        writer.append(record).unwrap();
    }
    let completion = writer.append(&history[2]).unwrap();
    let note = writer.append(&history[3]).unwrap();
    let mut decided = decision(if issue_contract == NoteContract::IdentifiedTaught {
        NoteContract::IdentifiedTaught
    } else {
        NoteContract::Taught
    });
    let (Record::TaughtNoteTurnDecided {
        note_lsn,
        completion_lsn,
        ..
    }
    | Record::IdentifiedTaughtNoteTurnDecided {
        note_lsn,
        completion_lsn,
        ..
    }) = &mut decided
    else {
        unreachable!()
    };
    *note_lsn = note;
    *completion_lsn = completion;
    let decision_lsn = writer.append(&entry(decided)).unwrap();
    let mut issued = issue(issue_contract);
    match &mut issued {
        Record::NoteOnlyTurnIssued {
            note_lsn,
            completion_lsn,
            lsn_of_decision,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            note_lsn,
            completion_lsn,
            lsn_of_decision,
            ..
        }
        | Record::IdentifiedTaughtNoteOnlyTurnIssued {
            note_lsn,
            completion_lsn,
            lsn_of_decision,
            ..
        } => {
            *note_lsn = note;
            *completion_lsn = completion;
            *lsn_of_decision = decision_lsn;
        }
        _ => unreachable!(),
    }
    let issue_lsn = writer.append(&entry(issued)).unwrap();
    (decision_lsn, issue_lsn)
}

#[test]
fn taught_only_invalid_prefix_refuses_before_repair_or_cache_change() {
    use std::io::Write as _;
    let owned = tempfile::tempdir().unwrap();
    let journal = owned.path().join("journal");
    append_joined_note_history(&journal, NoteContract::Raw);
    let tail = std::fs::read_dir(&journal)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .max()
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&tail)
        .unwrap()
        .write_all(b"\0\xff")
        .unwrap();
    let cache = owned.path().join("state.redb");
    let mut cached = Projections::open(&cache).unwrap();
    cached
        .apply(
            Lsn(u64::MAX - 1),
            17,
            &entry(Record::Checkpoint {
                segment: 1,
                offset: 1,
                projection_version: 36,
            }),
        )
        .unwrap();
    let expected_export = cached.export().unwrap();
    drop(cached);
    let before_journal = std::fs::read(&tail).unwrap();
    let before_cache = std::fs::read(&cache).unwrap();
    assert!(Reader::open(&journal).is_err());
    assert!(
        open(
            &journal,
            JournalOptions {
                fsync: Fsync::None,
                ..JournalOptions::default()
            }
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&tail).unwrap(), before_journal);
    assert_eq!(std::fs::read(&cache).unwrap(), before_cache);
    let mut cached = Projections::open(&cache).unwrap();
    assert_eq!(cached.export().unwrap(), expected_export);
    assert!(replay(&Reader::new(&journal), &mut cached).is_err());
    assert_eq!(cached.export().unwrap(), expected_export);
}

#[test]
fn taught_only_valid_prefix_rebuilds_from_the_first_record() {
    let owned = tempfile::tempdir().unwrap();
    let journal = owned.path().join("journal");
    append_joined_note_history(&journal, NoteContract::Taught);
    let reader = Reader::open(&journal).unwrap().0;
    let mut projected = Projections::in_memory().unwrap();
    assert_eq!(replay(&reader, &mut projected).unwrap().applied, 6);
    assert_eq!(projected.goal(goal()).unwrap().unwrap().turns_on_goal, 1);
    assert_eq!(projected.turn(turn()).unwrap().unwrap().goal, Some(goal()));
}

#[test]
fn identified_valid_prefix_rebuilds_from_the_first_record() {
    let owned = tempfile::tempdir().unwrap();
    let journal = owned.path().join("journal");
    append_joined_note_history(&journal, NoteContract::IdentifiedTaught);
    let reader = Reader::open(&journal).unwrap().0;
    let mut projected = Projections::in_memory().unwrap();
    assert_eq!(replay(&reader, &mut projected).unwrap().applied, 6);
    assert_eq!(projected.goal(goal()).unwrap().unwrap().turns_on_goal, 1);
    let frames: Vec<_> = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(|item| item.unwrap().1.record.kind())
        .collect();
    let new = [
        dudes_proto::RecordKind::IdentifiedTaughtNoteTurnDecided,
        dudes_proto::RecordKind::IdentifiedTaughtNoteOnlyTurnIssued,
    ];
    assert_eq!(frames.iter().filter(|kind| new.contains(kind)).count(), 2);
    assert!(
        new.iter()
            .all(|kind| kind.requires_required_frame() && kind.tag() > 94)
    );
}

#[test]
fn taught_issue_cannot_replace_an_existing_turn_or_steal_an_open_slot() {
    use dudes_proto::RecordKind;
    use dudes_proto::record::samples;
    for (open_slot, reuse, admitted) in [
        (false, true, false),
        (true, false, false),
        (false, false, true),
    ] {
        let mut history = seed();
        let prior_turn = if reuse {
            turn()
        } else {
            TurnId::from_parts(4, 4)
        };
        let mut prior = samples::record(RecordKind::TurnIssued);
        let Record::TurnIssued {
            agent: owner,
            goal: original,
            turn: prior_id,
            ..
        } = &mut prior
        else {
            unreachable!()
        };
        *owner = agent();
        *original = goal();
        *prior_id = prior_turn;
        history.push(entry(prior));
        if !open_slot {
            let mut ended = samples::record(RecordKind::TurnEnded);
            let Record::TurnEnded {
                agent: owner,
                turn: prior_id,
                ..
            } = &mut ended
            else {
                unreachable!()
            };
            *owner = agent();
            *prior_id = prior_turn;
            history.push(entry(ended));
        }
        let decision_lsn = Lsn(u64::try_from(history.len()).unwrap() + 1);
        history.push(entry(decision(NoteContract::Taught)));
        let mut projected = projected(&history);
        let before = projected.export().unwrap();
        let mut issued = issue(NoteContract::Taught);
        let Record::TaughtNoteOnlyTurnIssued {
            lsn_of_decision, ..
        } = &mut issued
        else {
            unreachable!()
        };
        *lsn_of_decision = decision_lsn;
        let result = projected.apply(Lsn(decision_lsn.0 + 1), 0, &entry(issued));
        if admitted {
            result.unwrap();
            assert_eq!(projected.goal(goal()).unwrap().unwrap().turns_on_goal, 2);
            assert_eq!(projected.turn(turn()).unwrap().unwrap().goal, Some(goal()));
        } else {
            assert!(result.is_err());
            assert_eq!(projected.export().unwrap(), before);
        }
    }
}

#[test]
fn taught_payload_requires_its_exact_required_header() {
    for (version, kind) in [(1, 63), (2, 6), (1, u16::MAX)] {
        let owned = tempfile::tempdir().unwrap();
        let journal = owned.path().join("journal");
        let (decision_lsn, _) = append_joined_note_history(&journal, NoteContract::Taught);
        let decision = Reader::new(&journal)
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .find(|(lsn, _)| *lsn == decision_lsn)
            .unwrap()
            .1;
        let path = std::fs::read_dir(&journal)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .max()
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let payload = serde_json::to_vec(&decision).unwrap();
        let mut frame = Vec::new();
        if version == 1 {
            dudes_journal::frame::encode(kind, &payload, &mut frame).unwrap();
        } else {
            dudes_journal::frame::encode_required(
                dudes_proto::RecordKind::TurnIssued,
                &payload,
                &mut frame,
            )
            .unwrap();
        }
        let mut bytes = original[..usize::try_from(decision_lsn.offset()).unwrap()].to_vec();
        bytes.extend(frame);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Reader::open(&journal), Err(dudes_journal::Error::TaughtNoteFrame { lsn, .. }) if lsn == decision_lsn)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
