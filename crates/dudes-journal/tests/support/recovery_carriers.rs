//! Independent ordinary issue prefixes used by both persistent and World allocation tests.

use dudes_proto::record::{BrowserNoteAttribution, Hex32, samples};
use dudes_proto::{
    BrowserNoteDestination, ConversationChargeScope, Gate, GateStep, LoopDecision, MailFrom,
    MailRef, MailTo, MailWakeAdmission, MailWakeDecision, MailWakeDigest, MailWakeDigestDomain,
    MailWakeIssue, MailWakePlan, MailWakePurpose, MailWakeTarget, MailWakeTask, NoteAdmission,
    Record, RecordKind,
};
use dudes_types::{AgentId, GoalId, Lsn, MailId, MailState, Mode, ModelSelection, TurnId};

#[derive(Clone, Copy, Debug)]
pub(super) enum Carrier {
    Conversation,
    Note,
    TaughtNote,
    MailWake,
}

pub(super) fn owner() -> AgentId {
    AgentId::from_parts(72, 1)
}
fn goal() -> GoalId {
    GoalId::from_parts(72, 2)
}
pub(super) fn unreserved_turn() -> TurnId {
    TurnId::from_parts(72, 3)
}

pub(super) fn set_turn(record: &mut Record, replacement: TurnId) {
    match record {
        Record::ConversationTurnIssued { turn, .. }
        | Record::NoteOnlyTurnIssued { turn, .. }
        | Record::TaughtNoteOnlyTurnIssued { turn, .. } => *turn = replacement,
        Record::MailWakeOnlyTurnIssued(issue) => issue.turn = replacement,
        _ => panic!("expected the prepared ordinary issue"),
    }
}

fn browser() -> BrowserNoteAttribution {
    BrowserNoteAttribution {
        request_id_digest: Hex32::new([21; 32]),
        request_digest: Hex32::new([22; 32]),
        session_digest: Hex32::new([23; 32]),
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

pub(super) fn candidate(
    carrier: Carrier,
    sender: AgentId,
    mut append: impl FnMut(Record) -> Lsn,
) -> Record {
    let mut hire = samples::record(RecordKind::AgentHired);
    if let Record::AgentHired { id, .. } = &mut hire {
        *id = owner();
    }
    let context = append(hire);
    match carrier {
        Carrier::Conversation => conversation(context, &mut append),
        Carrier::Note | Carrier::TaughtNote => {
            let completion = completed_goal(&mut append);
            note(
                completion,
                matches!(carrier, Carrier::TaughtNote),
                &mut append,
            )
        }
        Carrier::MailWake => {
            let completion = completed_goal(&mut append);
            wake(completion, sender, &mut append)
        }
    }
}

fn completed_goal(append: &mut impl FnMut(Record) -> Lsn) -> Lsn {
    let mut installed = samples::record(RecordKind::GoalSet);
    if let Record::GoalSet {
        agent,
        goal: target,
        ..
    } = &mut installed
    {
        *agent = owner();
        target.id = goal();
    }
    append(installed);
    let mut passed = samples::record(RecordKind::PredicateResult);
    if let Record::PredicateResult {
        goal: target, turn, ..
    } = &mut passed
    {
        *target = goal();
        *turn = None;
    }
    append(passed);
    append(Record::LoopDecided {
        agent: owner(),
        input_digest: "ordinary completion".into(),
        gate_trace: Vec::new(),
        decision: LoopDecision::Done,
    })
}

fn conversation(context: Lsn, append: &mut impl FnMut(Record) -> Lsn) -> Record {
    let text = "independent conversation";
    let note_lsn = append(Record::ConversationMessageQueued {
        agent: owner(),
        text: text.into(),
        browser: browser(),
        context_lsn: context,
        charge_scope: ConversationChargeScope::AgentAndFloor,
    });
    let decision = append(Record::ConversationTurnDecided {
        agent: owner(),
        note_lsn,
        context_lsn: context,
        charge_scope: ConversationChargeScope::AgentAndFloor,
        input_digest: "finite input".into(),
        gate_trace: Vec::new(),
        decision: NoteAdmission::Issue { task: text.into() },
    });
    Record::ConversationTurnIssued {
        agent: owner(),
        turn: unreserved_turn(),
        note_lsn,
        context_lsn: context,
        charge_scope: ConversationChargeScope::AgentAndFloor,
        task: text.into(),
        session: None,
        model_selection: ModelSelection::provider_default(),
        lsn_of_decision: decision,
    }
}

fn note(completion: Lsn, taught: bool, append: &mut impl FnMut(Record) -> Lsn) -> Record {
    let text = "independent note";
    let note_lsn = append(Record::BrowserNoteQueued {
        agent: owner(),
        text: text.into(),
        browser: browser(),
        destination: BrowserNoteDestination::ManagedGoal { goal: goal() },
    });
    let task = if taught {
        dudes_proto::taught_note_task(text).unwrap()
    } else {
        text.into()
    };
    let decision = if taught {
        Record::TaughtNoteTurnDecided {
            agent: owner(),
            goal: goal(),
            note_lsn,
            completion_lsn: completion,
            input_digest: "taught input".into(),
            gate_trace: trace(),
            decision: NoteAdmission::Issue { task: task.clone() },
        }
    } else {
        Record::NoteTurnDecided {
            agent: owner(),
            goal: goal(),
            note_lsn,
            completion_lsn: completion,
            input_digest: "raw input".into(),
            gate_trace: Vec::new(),
            decision: NoteAdmission::Issue { task: task.clone() },
        }
    };
    let lsn_of_decision = append(decision);
    if taught {
        Record::TaughtNoteOnlyTurnIssued {
            agent: owner(),
            turn: unreserved_turn(),
            goal: goal(),
            task,
            session: None,
            mode: Mode::Managed,
            model_selection: ModelSelection::provider_default(),
            lsn_of_decision,
            answers: Vec::new(),
            note_lsn,
            completion_lsn: completion,
        }
    } else {
        Record::NoteOnlyTurnIssued {
            agent: owner(),
            turn: unreserved_turn(),
            goal: goal(),
            task,
            session: None,
            mode: Mode::Managed,
            model_selection: ModelSelection::provider_default(),
            lsn_of_decision,
            answers: Vec::new(),
            note_lsn,
            completion_lsn: completion,
        }
    }
}

fn wake_decision(
    completion: Lsn,
    purpose: MailWakePurpose,
    domain: MailWakeDigestDomain,
) -> Record {
    Record::MailWakeTurnDecided(MailWakeDecision {
        agent: owner(),
        goal: goal(),
        completion_lsn: completion,
        purpose,
        input_digest: MailWakeDigest {
            domain,
            value: Hex32::new([24; 32]),
        },
        gate_trace: trace(),
        decision: MailWakeAdmission::Issue {
            task: MailWakeTask::new(1).unwrap(),
        },
    })
}

fn wake(completion: Lsn, sender: AgentId, append: &mut impl FnMut(Record) -> Lsn) -> Record {
    let mut mail = samples::record(RecordKind::Mail);
    let mail_id = MailId::from_parts(72, 4);
    if let Record::Mail {
        id,
        from,
        to,
        state,
        ..
    } = &mut mail
    {
        *id = mail_id;
        *from = MailFrom::Agent { agent: sender };
        *to = MailTo::Agent { agent: owner() };
        *state = MailState::Durable;
    }
    let lsn = append(mail);
    let receipt = MailRef { id: mail_id, lsn };
    let root = append(wake_decision(
        completion,
        MailWakePurpose::FreshPreparation {
            plan: MailWakePlan {
                trigger: receipt,
                frontier: lsn,
                unread_count: 1,
                task: MailWakeTask::new(1).unwrap(),
                targets: vec![MailWakeTarget {
                    mail: receipt,
                    original_count: 0,
                    target_count: 1,
                }],
            },
        },
        MailWakeDigestDomain::Fresh,
    ));
    append(Record::MailState {
        id: mail_id,
        state: MailState::Notified { n: 1 },
    });
    let decision = append(wake_decision(
        completion,
        MailWakePurpose::ExistingPreparation { root_lsn: root },
        MailWakeDigestDomain::Resume,
    ));
    Record::MailWakeOnlyTurnIssued(MailWakeIssue {
        agent: owner(),
        goal: goal(),
        turn: unreserved_turn(),
        completion_lsn: completion,
        root_lsn: root,
        lsn_of_decision: decision,
        task: MailWakeTask::new(1).unwrap(),
        session: None,
        mode: Mode::Managed,
        model_selection: ModelSelection::provider_default(),
        answers: Vec::new(),
    })
}
