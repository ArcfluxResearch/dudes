#![allow(missing_docs)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dudes_journal::{ChatOverlay, JournalOptions, Projections, Reader, Writer, open, replay};
use dudes_proto::{ChatPendingState, ChatTurnState, Entry};
use dudes_supervisor::World;
use dudes_supervisor::world::{BrowserIssueCancellation, BrowserNoteState};
use dudes_types::{AgentId, GoalId, GoalStatus, Lsn, Timestamp, TurnId};
use serde_json::{Value, json};

fn agent() -> AgentId {
    AgentId::from_parts(1, 1)
}
fn goal() -> GoalId {
    GoalId::from_parts(2, 1)
}
fn turn(id: u64) -> TurnId {
    TurnId::from_parts(3, u128::from(id))
}

struct Prefix {
    dir: tempfile::TempDir,
    writer: Writer,
    projected: Projections,
    world: World,
    count: usize,
    request_ids: BTreeSet<[u8; 32]>,
}

struct DaemonReplay {
    world: World,
    projected: Projections,
    _state: tempfile::TempDir,
    _runtime: tempfile::TempDir,
}

impl Prefix {
    fn new() -> Self {
        Self::with_fsync(dudes_journal::Fsync::Full)
    }

    fn with_fsync(fsync: dudes_journal::Fsync) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (writer, _) = open(
            &dir.path().join("journal"),
            JournalOptions {
                fsync,
                checkpoint_every: 0,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        let mut prefix = Self {
            dir,
            writer,
            projected: Projections::in_memory().unwrap(),
            world: World::default(),
            count: 0,
            request_ids: BTreeSet::new(),
        };
        prefix.append(json!({"t":"agent_hired","id":agent(),"name":"Owned test agent","role":"test","department":"test","provider":"claude","cwd":prefix.dir.path(),"budget":{}}));
        prefix.append(json!({"t":"goal_set","agent":agent(),"goal":{"id":goal(),"text":"Complete owned work","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":50}}));
        prefix
    }

    fn append(&mut self, value: Value) -> Lsn {
        let lsn = self.fold(value);
        self.assert_replayed();
        lsn
    }

    fn fold(&mut self, value: Value) -> Lsn {
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: serde_json::from_value(value).unwrap(),
        };
        let (lsn, crc) = self.writer.append_witnessed(&entry).unwrap();
        self.projected.apply(lsn, crc, &entry).unwrap();
        self.world.apply(lsn, &entry);
        self.count += 1;
        let browser = match &entry.record {
            dudes_proto::Record::BrowserNoteQueued { browser, .. }
            | dudes_proto::Record::BrowserNoteCancelled { browser, .. }
            | dudes_proto::Record::BrowserNoteIssuedCancelled { browser, .. }
            | dudes_proto::Record::BrowserNoteReissueCancelled { browser, .. } => Some(browser),
            _ => None,
        };
        if let Some(browser) = browser {
            assert!(
                self.request_ids.insert(browser.request_id_digest.bytes()),
                "each successful seeded browser request needs a unique identity"
            );
        }
        if entry.record.kind().tag() >= 47 {
            let path = dudes_journal::segment::segment_path(
                &self.dir.path().join("journal"),
                lsn.segment(),
            );
            let bytes = std::fs::read(path).unwrap();
            let start = usize::try_from(lsn.offset()).unwrap();
            let header =
                dudes_journal::frame::Header::parse(bytes[start..start + 12].try_into().unwrap());
            assert_eq!(header.ver, 2, "every Chat record requires understanding");
        }
        lsn
    }

    fn assert_replayed(&self) {
        let (reader, _) = Reader::open(&self.dir.path().join("journal")).unwrap();
        let mut rebuilt = Projections::in_memory().unwrap();
        replay(&reader, &mut rebuilt).unwrap();
        assert_eq!(
            rebuilt.export().unwrap(),
            self.projected.export().unwrap(),
            "logical export at acknowledged prefix {}",
            self.count
        );
        assert_eq!(rebuilt.chat().unwrap(), self.projected.chat().unwrap());
        let mut world = World::default();
        for entry in reader.iter_from(Lsn::ZERO).unwrap() {
            let (position, entry) = entry.unwrap();
            world.apply(position, &entry);
        }
        assert_eq!(
            format!("{world:?}"),
            format!("{:?}", self.world),
            "World at acknowledged prefix {}",
            self.count
        );
    }

    fn daemon_replayed(&self) -> anyhow::Result<DaemonReplay> {
        let state = tempfile::tempdir()?;
        let journal = state.path().join("journal");
        std::fs::create_dir(&journal)?;
        for file in std::fs::read_dir(self.dir.path().join("journal"))? {
            let file = file?;
            std::fs::copy(file.path(), journal.join(file.file_name()))?;
        }
        if !self.world.agents.is_empty() {
            let (mut writer, _) = open(
                &journal,
                JournalOptions {
                    checkpoint_every: 0,
                    ..JournalOptions::default()
                },
            )?;
            for agent in self.world.agents.keys() {
                writer.append(&Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: dudes_proto::Record::Halt {
                        agent: *agent,
                        text: Some(String::from("owned replay-only control")),
                    },
                })?;
            }
        }
        let runtime = tempfile::tempdir()?;
        let host = common::SharedHost::new();
        for row in self.world.turns.values() {
            if let Some(pid) = row.pid {
                host.answer(pid, dudes_supervisor::host::ProbeAnswer::CouldNotAnswer);
            }
        }
        let runner = Arc::new(common::ScriptedRunner::default());
        let settings = common::settings_at(state.path(), runtime.path());
        let daemon = duded::boot(
            &settings,
            common::seams(
                &settings,
                host.clone(),
                runner.clone(),
                Arc::new(common::TestClock::new()),
                duded::startup::Witness::default(),
            ),
        )?;
        let world = daemon.world().clone();
        assert!(host.spawned().is_empty());
        assert_eq!(runner.runs(), 0);
        drop(daemon);
        let projected = Projections::open(&state.path().join(duded::boot::PROJECTION_FILE))?;
        let (reader, _) = Reader::open(&journal)?;
        let mut fresh = Projections::in_memory()?;
        replay(&reader, &mut fresh)?;
        assert_eq!(fresh.export()?, projected.export()?);
        Ok(DaemonReplay {
            world,
            projected,
            _state: state,
            _runtime: runtime,
        })
    }

    fn queue(&mut self, text: &str, attached: bool) -> Lsn {
        let browser = self.attribution();
        self.append(json!({"t":"browser_note_queued","agent":agent(),"text":text,
            "browser":browser,
            "destination":if attached { json!({"kind":"attached_context"}) } else { json!({"kind":"managed_goal","goal":goal()}) }
        }))
    }

    fn decide(&mut self, task: &str) -> Lsn {
        self.append(json!({"t":"loop_decided","agent":agent(),"input_digest":"ordinary","decision":{"kind":"continue","task":task},"gate_trace":[]}))
    }

    fn complete(&mut self) -> Lsn {
        self.append(json!({"t":"predicate_result","agent":agent(),"goal":goal(),"provenance":"observed","pass":true,"evidence":{"kind":"command","argv":["/usr/bin/true"],"program":"/usr/bin/true","exit":0}}));
        self.append(json!({"t":"loop_decided","agent":agent(),"input_digest":"ordinary-done","decision":{"kind":"done"},"gate_trace":[]}))
    }

    fn note_decide(&mut self, note: Lsn, done: Lsn, text: &str) -> Lsn {
        self.append(json!({"t":"note_turn_decided","agent":agent(),"goal":goal(),"note_lsn":note,"completion_lsn":done,"input_digest":"completed-note","gate_trace":[],"decision":{"kind":"issue","task":text}}))
    }

    fn issue(
        &mut self,
        id: TurnId,
        note: Lsn,
        decision: Lsn,
        done: Option<Lsn>,
        task: &str,
        answers: &[Lsn],
    ) {
        let mut value = json!({"t":if done.is_some(){"note_only_turn_issued"}else{"turn_issued_with_browser_note"},
            "agent":agent(),"turn":id,"goal":goal(),"task":task,"session":null,"mode":"managed",
            "model_selection":{},"lsn_of_decision":decision,"answers":answers,"note_lsn":note});
        if let Some(done) = done {
            value["completion_lsn"] = json!(done);
        }
        self.append(value);
    }

    fn intent(&mut self, id: TurnId, note: Lsn) -> Lsn {
        self.append(
            json!({"t":"browser_note_dispatch_intent","agent":agent(),"turn":id,"note_lsn":note}),
        )
    }

    fn spawn(&mut self, id: TurnId) {
        self.append(json!({"t":"turn_spawned","agent":agent(),"turn":id,"pid":4242,"start_time":1,"session":null,"reissued":false}));
    }

    fn end(&mut self, id: TurnId, reply: Option<&str>) {
        self.append(json!({"t":"turn_ended","agent":agent(),"turn":id,"mode":"managed","provenance":"observed",
            "exit":{"code":0,"signal":null},"outcome":{"kind":"completed"},"last_message":reply,"usage":{"input":10,"output":5,"usd":7}}));
    }

    fn attribution(&self) -> Value {
        let digest = format!("{:016x}", self.count).repeat(4);
        json!({"request_id_digest":digest,"request_digest":"22".repeat(32),"session_digest":"33".repeat(32)})
    }
}

#[test]
fn ordinary_note_reserves_identity_and_consumes_only_carried_answers_at_spawn() {
    let mut p = Prefix::new();
    p.append(json!({"t":"human_question","agent":agent(),"text":"first question"}));
    let first = p.append(json!({"t":"human_answer","agent":agent(),"text":"carried answer"}));
    p.append(json!({"t":"human_question","agent":agent(),"text":"second question"}));
    let second = p.append(json!({"t":"human_answer","agent":agent(),"text":"saved answer"}));
    let note = p.queue("the browser note", false);
    let other = p.queue("another note", false);
    let task = "carried answer\nComplete owned work\n\n[browser note]\nthe browser note\n[end browser note]";
    let decision = p.decide(task);
    p.issue(turn(1), note, decision, None, task, &[first]);
    assert_eq!(p.world.agents[&agent()].pending_answers.len(), 2);
    let pending = p.projected.chat().unwrap().pending;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].state, ChatPendingState::Reserved);
    assert_eq!(pending[0].note_lsn, note.0.to_string());
    p.append(json!({"t":"browser_note_cancelled","agent":agent(),"note_lsn":other,"browser":p.attribution()}));
    p.append(json!({"t":"hook_token_minted","agent":agent(),"token_hash":"11".repeat(32)}));
    assert_eq!(p.projected.chat().unwrap().pending.len(), 1);
    assert!(matches!(
        p.world.browser_note(note).unwrap().state,
        BrowserNoteState::Reserved { .. }
    ));
    p.intent(turn(1), note);
    let mut overlays = BTreeMap::new();
    overlays.insert(
        note,
        ChatOverlay {
            uncertain: false,
            delivery_started: false,
            reason: None,
        },
    );
    assert_eq!(
        p.projected.chat_with_overlays(&overlays).unwrap().pending[0].state,
        ChatPendingState::Reserved
    );
    overlays.insert(
        note,
        ChatOverlay {
            uncertain: false,
            delivery_started: true,
            reason: None,
        },
    );
    let started = p.projected.chat_with_overlays(&overlays).unwrap();
    assert!(started.pending.is_empty());
    assert_eq!(started.turns[0].state, ChatTurnState::Running);
    assert_eq!(
        p.projected.chat().unwrap().pending.len(),
        1,
        "durable quota reservation survives the live overlay"
    );
    assert_eq!(
        p.world.agents[&agent()].pending_answers.len(),
        2,
        "host-call knowledge is not durable spawn consumption"
    );
    p.spawn(turn(1));
    assert!(p.projected.chat().unwrap().pending.is_empty());
    assert_eq!(
        p.world.agents[&agent()]
            .pending_answers
            .front()
            .unwrap()
            .lsn,
        second
    );
    p.end(turn(1), Some("finished naturally..."));
    let chat = p.projected.chat().unwrap();
    assert_eq!(
        chat.turns[0].reply.as_deref(),
        Some("finished naturally...")
    );
    assert_eq!(chat.turns[0].truncated, None);
    assert!(p.world.completed_goal(agent()).is_none());
    assert_eq!(
        p.projected.spend_by_goal(goal()).unwrap().tokens.count(),
        15
    );
}

#[test]
fn note_only_spawn_keeps_saved_answers_and_original_completion() {
    let mut p = Prefix::new();
    p.append(json!({"t":"human_question","agent":agent(),"text":"saved question"}));
    let saved =
        p.append(json!({"t":"human_answer","agent":agent(),"text":"future ordinary answer"}));
    let note = p.queue("note alone", false);
    let done = p.complete();
    let decision = p.note_decide(note, done, "note alone");
    p.issue(turn(2), note, decision, Some(done), "note alone", &[]);
    p.intent(turn(2), note);
    p.spawn(turn(2));
    assert_eq!(
        p.world.agents[&agent()]
            .pending_answers
            .front()
            .unwrap()
            .lsn,
        saved
    );
    assert_eq!(p.world.completed_goal(agent()), Some((goal(), done)));
    assert_eq!(
        p.world.agents[&agent()].last_input_digest.as_deref(),
        Some("ordinary-done")
    );
    assert_eq!(
        p.projected.goal(goal()).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    p.end(turn(2), None);
    assert_eq!(p.world.completed_goal(agent()), Some((goal(), done)));
    assert_eq!(
        p.world.agents[&agent()]
            .pending_answers
            .front()
            .unwrap()
            .lsn,
        saved
    );
    assert_eq!(p.projected.chat().unwrap().turns[0].reply, None);
    assert_eq!(p.projected.goal(goal()).unwrap().unwrap().turns_on_goal, 1);
    assert_eq!(
        p.projected.spend_by_goal(goal()).unwrap().tokens.count(),
        15
    );
    let activity = serde_json::to_value(p.projected.activity().unwrap()).unwrap();
    assert_eq!(
        activity
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "goal_done")
            .count(),
        1
    );
}

#[test]
fn clean_issue_cancellation_keeps_the_hop_and_never_fabricates_an_end() {
    let mut p = Prefix::new();
    let note = p.queue("never dispatched", false);
    let done = p.complete();
    let decision = p.note_decide(note, done, "never dispatched");
    p.issue(turn(3), note, decision, Some(done), "never dispatched", &[]);
    let last_end = p.world.agents[&agent()].last_turn_end;
    p.append(json!({"t":"browser_note_issued_cancelled","agent":agent(),"note_lsn":note,"turn":turn(3),"browser":p.attribution()}));
    assert!(p.world.browser_issue_cancelled(turn(3)));
    assert!(p.world.issued_not_spawned().next().is_none());
    assert!(p.projected.open_turns().unwrap().is_empty());
    assert!(p.world.agents[&agent()].open_turn.is_none());
    assert_eq!(p.world.agents[&agent()].last_turn_end, last_end);
    assert_eq!(p.world.completed_goal(agent()), Some((goal(), done)));
    assert_eq!(p.world.goals[&goal()].turns_on_goal, 1);
    assert_eq!(
        p.projected.chat().unwrap().turns[0].state,
        ChatTurnState::Cancelled
    );
    assert_eq!(p.projected.spend_by_goal(goal()).unwrap().tokens.count(), 0);
    p.append(json!({"t":"closing_time","phase":dudes_proto::ClosingPhase::Announced}));
    p.append(json!({"t":"closing_time","phase":dudes_proto::ClosingPhase::Complete}));
    assert!(
        !p.world.closing.as_ref().unwrap().needs_recovery(),
        "a clean cancelled turn is not an owned closure resource"
    );
}

#[test]
fn ambiguous_reissue_cancellation_retains_every_historical_intent_and_open_turn() {
    let mut p = Prefix::new();
    let note = p.queue("possibly delivered", false);
    let decision = p.decide("possibly delivered");
    p.issue(turn(4), note, decision, None, "possibly delivered", &[]);
    let first = p.intent(turn(4), note);
    let latest = p.intent(turn(4), note);
    p.append(json!({"t":"browser_note_reissue_cancelled","agent":agent(),"note_lsn":note,"turn":turn(4),"dispatch_intent_lsn":first,"browser":p.attribution()}));
    let issue = p.world.browser_issue(turn(4)).unwrap();
    assert_eq!(issue.dispatch_intents, [first, latest]);
    assert!(matches!(
        issue.cancellation,
        Some(BrowserIssueCancellation::Ambiguous { .. })
    ));
    assert_eq!(p.world.agents[&agent()].open_turn, Some(turn(4)));
    assert!(p.world.issued_not_spawned().next().is_none());
    assert_eq!(p.projected.open_turns().unwrap().len(), 1);
    let mut overlays = BTreeMap::new();
    overlays.insert(
        note,
        ChatOverlay {
            uncertain: false,
            delivery_started: false,
            reason: None,
        },
    );
    let view = p.projected.chat_with_overlays(&overlays).unwrap();
    assert_eq!(view.pending[0].state, ChatPendingState::DeliveryUncertain);
    assert!(!view.pending[0].cancellable);
    assert_eq!(view.turns[0].state, ChatTurnState::ReissueCancelled);
}

#[test]
fn attached_emission_is_a_bodyless_context_receipt_and_retirement_hides_it() {
    let mut p = Prefix::new();
    p.append(json!({"t":"attached","agent":agent(),"by":{"kind":"system"}}));
    let note = p.queue("attached private body", true);
    p.append(json!({"t":"browser_note_context_emitted","agent":agent(),"note_lsn":note}));
    let view = p.projected.chat().unwrap();
    assert!(view.pending.is_empty());
    assert!(view.turns.is_empty());
    assert_eq!(view.context_sent.len(), 1);
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("attached private body")
    );
    assert!(matches!(
        p.world.browser_note(note).unwrap().state,
        BrowserNoteState::ContextEmitted
    ));
    p.append(json!({"t":"agent_retired","id":agent(),"reason":"owned retirement"}));
    assert!(p.projected.chat().unwrap().context_sent.is_empty());
}

#[test]
fn escaped_legacy_replies_remain_visible_within_the_larger_history_budget() {
    let mut p = Prefix::new();
    let done = p.complete();
    let old = p.queue("old fitting note", false);
    let decision = p.note_decide(old, done, "old fitting note");
    p.issue(turn(20), old, decision, Some(done), "old fitting note", &[]);
    p.intent(turn(20), old);
    p.spawn(turn(20));
    p.end(turn(20), Some("old fitting reply"));
    let escaped = format!("x{}x", "\n".repeat(4094));
    let note = p.queue(&escaped, false);
    let decision = p.note_decide(note, done, &escaped);
    p.issue(turn(21), note, decision, Some(done), &escaped, &[]);
    p.intent(turn(21), note);
    p.spawn(turn(21));
    p.end(turn(21), Some(&"\0".repeat(4096)));
    let view = p.projected.chat().unwrap();
    assert_eq!(view.turns.len(), 2);
    assert_eq!(view.turns[0].turn_id, turn(20));
    assert_eq!(view.turns[0].reply.as_deref(), Some("old fitting reply"));
    assert!(view.pending.is_empty());
    assert_eq!(
        serde_json::to_value(&view)
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn reply_truncation_is_utf8_safe_and_exact_length_never_invents_completeness() {
    let mut p = Prefix::new();
    let done = p.complete();
    for (id, reply) in [
        (30, format!("{}...", "x".repeat(4093))),
        (31, "🦀".repeat(1025)),
    ] {
        let note = p.queue("reply test", false);
        let decision = p.note_decide(note, done, "reply test");
        p.issue(turn(id), note, decision, Some(done), "reply test", &[]);
        p.intent(turn(id), note);
        p.spawn(turn(id));
        p.end(turn(id), Some(&reply));
    }
    let view = p.projected.chat().unwrap();
    assert_eq!(view.turns[0].reply.as_ref().unwrap().len(), 4096);
    assert_eq!(view.turns[0].truncated, None);
    assert_eq!(view.turns[1].reply.as_ref().unwrap(), &"🦀".repeat(1024));
    assert_eq!(view.turns[1].truncated, Some(true));
}

#[test]
fn spawn_consumes_the_reserved_note_instead_of_an_older_ineligible_queue_front() {
    let mut p = Prefix::new();
    p.append(json!({"t":"attached","agent":agent(),"by":{"kind":"system"}}));
    let old = p.queue("old attached note", true);
    p.append(json!({"t":"detached","agent":agent(),"by":{"kind":"system"}}));
    let note = p.queue("current managed note", false);
    assert_eq!(
        p.world
            .eligible_browser_note(
                agent(),
                &dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal() }
            )
            .unwrap()
            .0,
        note
    );
    let decision = p.decide("current managed note");
    p.issue(turn(40), note, decision, None, "current managed note", &[]);
    p.intent(turn(40), note);
    p.spawn(turn(40));
    let pending = p.projected.chat().unwrap().pending;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].note_lsn, old.0.to_string());
    assert!(matches!(
        p.world.browser_note(old).unwrap().state,
        BrowserNoteState::Queued
    ));
    assert!(matches!(
        p.world.browser_note(note).unwrap().state,
        BrowserNoteState::Delivered { .. }
    ));
}

#[test]
fn pending_admission_reserves_escaped_overlays_and_never_evicts_accepted_notes() {
    let mut p = Prefix::new();
    let text = format!("x{}x", "\n".repeat(3000));
    for _ in 0..20 {
        p.queue(&text, false);
    }
    let view = p.projected.chat().unwrap();
    assert_eq!(view.pending.len(), 20);
    let candidate = dudes_proto::ChatPendingView {
        agent_id: AgentId::from_parts(1, 2),
        note_lsn: u64::MAX.to_string(),
        goal_id: Some(GoalId::from_parts(2, 2)),
        text,
        state: ChatPendingState::Queued,
        turn_id: None,
        cancellable: true,
        reason: None,
    };
    assert!(
        !dudes_journal::admits_pending(&view.pending, &candidate).unwrap(),
        "serialized bytes bind before global count or candidate agent count"
    );
    let mut overlays = BTreeMap::new();
    for row in &view.pending {
        overlays.insert(
            Lsn(row.note_lsn.parse().unwrap()),
            ChatOverlay {
                uncertain: false,
                delivery_started: false,
                reason: Some(dudes_proto::ChatReason::UnsupportedDelivery),
            },
        );
    }
    let overlaid = p.projected.chat_with_overlays(&overlays).unwrap();
    assert_eq!(overlaid.pending.len(), 20);
    assert!(
        serde_json::to_vec(&overlaid.pending).unwrap().len() <= dudes_proto::MAX_CHAT_PENDING_BYTES
    );
    assert!(
        serde_json::to_vec(&overlaid).unwrap().len() + b",\"chat\":".len()
            <= dudes_proto::MAX_CHAT_BYTES
    );
}

#[test]
fn the_exact_context_contract_leaves_room_below_its_allocated_byte_reservation() {
    let row = dudes_proto::ChatContextView {
        agent_id: AgentId::from_parts(1, 1),
        note_lsn: u64::MAX.to_string(),
        state: dudes_proto::ChatContextState::SentAsContext,
    };
    assert_eq!(serde_json::to_vec(&row).unwrap().len(), 101);
    let bytes = serde_json::to_vec(&vec![row; 128]).unwrap().len();
    assert_eq!(bytes, 13_057);
    assert!(bytes < dudes_proto::MAX_CHAT_CONTEXT_BYTES);
}

#[test]
fn a_failed_launch_is_terminal_without_delivery_or_automatic_requeue() {
    let mut p = Prefix::new();
    let note = p.queue("could not launch", false);
    let decision = p.decide("could not launch");
    p.issue(turn(50), note, decision, None, "could not launch", &[]);
    p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(50),"mode":"managed","provenance":"observed",
        "exit":{"code":null,"signal":null},"outcome":{"kind":"not_launched","reason":"owned launch failure"},"last_message":null,"usage":{}}));
    assert!(matches!(
        p.world.browser_note(note).unwrap().state,
        BrowserNoteState::Failed { .. }
    ));
    assert!(p.world.browser_issue(turn(50)).unwrap().spawn_lsn.is_none());
    assert!(
        p.world
            .eligible_browser_note(
                agent(),
                &dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal() }
            )
            .is_none()
    );
    let view = p.projected.chat().unwrap();
    assert!(view.pending.is_empty());
    assert_eq!(view.turns[0].state, ChatTurnState::Failed);
    assert_eq!(view.turns[0].reply, None);
    assert_eq!(view.turns[0].truncated, None);
}

#[test]
fn a_consumed_note_cannot_join_a_second_issue_even_at_a_new_decision() {
    let mut p = Prefix::new();
    let note = p.queue("once only", false);
    let decision = p.decide("once only");
    p.issue(turn(60), note, decision, None, "once only", &[]);
    p.intent(turn(60), note);
    p.spawn(turn(60));
    p.end(turn(60), Some("first reply"));
    let decision = p.decide("once only");
    let invalid = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({
        "t":"turn_issued_with_browser_note","agent":agent(),"turn":turn(61),"goal":goal(),"task":"once only",
        "session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note
    })).unwrap() };
    let before = p.projected.export().unwrap();
    let (position, crc) = p.writer.append_witnessed(&invalid).unwrap();
    assert!(
        p.projected.apply(position, crc, &invalid).is_err(),
        "the durable cache refuses a duplicate note join"
    );
    assert_eq!(
        p.projected.export().unwrap(),
        before,
        "rejected fold is transactional"
    );
    p.world.apply(position, &invalid);
    assert!(!p.world.turns.contains_key(&turn(61)));
    assert_eq!(p.world.goals[&goal()].turns_on_goal, 1);
    let (reader, _) = Reader::open(&p.dir.path().join("journal")).unwrap();
    let mut fresh = Projections::in_memory().unwrap();
    assert!(
        replay(&reader, &mut fresh).is_err(),
        "replay refuses the same invalid join"
    );
    assert_eq!(
        fresh.last_applied_lsn(),
        Lsn::ZERO,
        "the rejected replay batch commits no cursor or rows"
    );
}

#[test]
fn unrelated_managed_and_attached_ends_never_supply_a_chat_reply() {
    let mut p = Prefix::new();
    let note = p.queue("my turn", false);
    let decision = p.decide("my turn");
    p.issue(turn(70), note, decision, None, "my turn", &[]);
    p.intent(turn(70), note);
    p.spawn(turn(70));
    p.end(turn(70), Some("my durable final"));
    let original = p.projected.chat().unwrap();
    let decision = p.decide("unrelated ordinary work");
    p.append(json!({"t":"turn_issued","agent":agent(),"turn":turn(71),"goal":goal(),"task":"unrelated ordinary work","session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[]}));
    p.spawn(turn(71));
    p.end(turn(71), Some("unrelated managed secret"));
    assert_eq!(p.projected.chat().unwrap(), original);
    assert!(!p.projected.turn(turn(71)).unwrap().unwrap().is_open());
    p.append(json!({"t":"attached","agent":agent(),"by":{"kind":"system"}}));
    let attached = p.queue("attached context", true);
    p.append(json!({"t":"browser_note_context_emitted","agent":agent(),"note_lsn":attached}));
    let with_context = p.projected.chat().unwrap();
    p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(72),"mode":"attached","provenance":"observed",
        "exit":{"code":0},"outcome":{"kind":"completed"},"last_message":"attached transcript secret","usage":{}}));
    assert_eq!(p.projected.chat().unwrap(), with_context);
    assert!(!p.projected.turn(turn(72)).unwrap().unwrap().is_open());
    assert_eq!(
        with_context.turns[0].reply.as_deref(),
        Some("my durable final")
    );
    assert!(
        !serde_json::to_string(&with_context)
            .unwrap()
            .contains("secret")
    );
}

fn bounded_owner(p: &mut Prefix, serial: u64) -> (AgentId, GoalId) {
    let agent = AgentId::from_parts(1, u128::from(serial));
    let goal = GoalId::from_parts(2, u128::from(serial));
    p.fold(json!({"t":"agent_hired","id":agent,"name":"Bounded owned agent","role":"test","department":"test","provider":"claude","cwd":p.dir.path(),"budget":{}}));
    p.fold(json!({"t":"goal_set","agent":agent,"goal":{"id":goal,"text":"Completed owned work","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":50}}));
    p.fold(json!({"t":"predicate_result","agent":agent,"goal":goal,"provenance":"observed","pass":true,"evidence":{"kind":"command","argv":["/usr/bin/true"],"program":"/usr/bin/true","exit":0}}));
    p.fold(json!({"t":"loop_decided","agent":agent,"input_digest":"ordinary-done","decision":{"kind":"done"},"gate_trace":[]}));
    (agent, goal)
}

fn bounded_queue(p: &mut Prefix, owner: (AgentId, GoalId), text: &str, attached: bool) -> Lsn {
    p.fold(json!({"t":"browser_note_queued","agent":owner.0,"text":text,"browser":p.attribution(),
        "destination":if attached { json!({"kind":"attached_context"}) } else { json!({"kind":"managed_goal","goal":owner.1}) }}))
}

fn bounded_turn(p: &mut Prefix, owner: (AgentId, GoalId), serial: u64, text: &str, reply: &str) {
    let note = bounded_queue(p, owner, text, false);
    let done = p.world.completed_goal(owner.0).unwrap().1;
    let decision = p.fold(json!({"t":"note_turn_decided","agent":owner.0,"goal":owner.1,"note_lsn":note,"completion_lsn":done,"input_digest":"bounded-admission","gate_trace":[],"decision":{"kind":"issue","task":text}}));
    let turn = turn(serial);
    p.fold(json!({"t":"note_only_turn_issued","agent":owner.0,"turn":turn,"goal":owner.1,"task":text,"session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note,"completion_lsn":done}));
    p.fold(json!({"t":"browser_note_dispatch_intent","agent":owner.0,"turn":turn,"note_lsn":note}));
    p.fold(json!({"t":"turn_spawned","agent":owner.0,"turn":turn,"pid":4242,"start_time":1,"session":null,"reissued":false}));
    p.fold(json!({"t":"turn_ended","agent":owner.0,"turn":turn,"mode":"managed","provenance":"observed","exit":{"code":0},"outcome":{"kind":"completed"},"last_message":reply,"usage":{}}));
}

fn fill_global_pending(p: &mut Prefix, owners: &[(AgentId, GoalId)]) {
    let mut text = String::from("x");
    loop {
        let candidate = dudes_proto::ChatPendingView {
            agent_id: owners[0].0,
            note_lsn: u64::MAX.to_string(),
            goal_id: Some(owners[0].1),
            text: format!("{text}\nx"),
            state: ChatPendingState::Queued,
            turn_id: None,
            cancellable: true,
            reason: None,
        };
        if candidate.reserved_bytes().unwrap() > (dudes_proto::MAX_CHAT_PENDING_BYTES - 129) / 128 {
            break;
        }
        text.push_str("\nx");
    }
    for index in 0..128 {
        bounded_queue(p, owners[index % owners.len()], &text, false);
    }
}

fn assert_serialized_chat_bounds(view: &dudes_proto::ChatView) {
    assert!(
        serde_json::to_vec(&view.pending).unwrap().len() <= dudes_proto::MAX_CHAT_PENDING_BYTES
    );
    assert!(
        serde_json::to_vec(&view.context_sent).unwrap().len()
            <= dudes_proto::MAX_CHAT_CONTEXT_BYTES
    );
    assert!(
        serde_json::to_vec(view).unwrap().len() + b",\"chat\":".len()
            <= dudes_proto::MAX_CHAT_BYTES
    );
}

#[test]
fn chat_global_counts_and_escaped_bytes_bound_all_arrays_without_dropping_pending() {
    let mut p = Prefix::with_fsync(dudes_journal::Fsync::None);
    let owners: Vec<_> = (2..11).map(|id| bounded_owner(&mut p, id)).collect();
    for (index, owner) in owners.iter().enumerate() {
        for round in 0..9 {
            bounded_turn(
                &mut p,
                *owner,
                100 + u64::try_from(index * 9 + round).unwrap(),
                "count note",
                "count reply",
            );
        }
    }
    let view = p.projected.chat().unwrap();
    assert_eq!(view.turns.len(), dudes_proto::MAX_CHAT_TURNS);
    for owner in &owners {
        assert!(
            view.turns
                .iter()
                .filter(|row| row.agent_id == owner.0)
                .count()
                <= dudes_proto::MAX_CHAT_TURNS_PER_AGENT
        );
    }
    p.assert_replayed();
    for owner in owners.iter().take(7) {
        p.fold(json!({"t":"attached","agent":owner.0,"by":{"kind":"system"}}));
        for _ in 0..21 {
            let note = bounded_queue(&mut p, *owner, "context", true);
            p.fold(json!({"t":"browser_note_context_emitted","agent":owner.0,"note_lsn":note}));
        }
        p.fold(json!({"t":"detached","agent":owner.0,"by":{"kind":"system"}}));
    }
    for (index, owner) in owners.iter().take(6).enumerate() {
        for round in 0..2 {
            bounded_turn(
                &mut p,
                *owner,
                300 + u64::try_from(index * 2 + round).unwrap(),
                &"\0".repeat(dudes_proto::MAX_CHAT_TEXT_BYTES),
                &"\0".repeat(dudes_proto::MAX_CHAT_TEXT_BYTES),
            );
        }
    }
    let history = p.projected.chat().unwrap();
    assert!(
        history.turns.len() < dudes_proto::MAX_CHAT_TURNS,
        "escaped bytes bind before the global count"
    );
    assert_eq!(history.context_sent.len(), dudes_proto::MAX_CHAT_CONTEXT);
    assert!(serde_json::to_vec(&history.turns).unwrap().len() <= dudes_proto::MAX_CHAT_TURN_BYTES);
    for owner in &owners {
        let rows: Vec<_> = history
            .turns
            .iter()
            .filter(|row| row.agent_id == owner.0)
            .collect();
        assert!(
            serde_json::to_vec(&rows).unwrap().len() <= dudes_proto::MAX_CHAT_TURN_BYTES_PER_AGENT
        );
        assert!(
            history
                .context_sent
                .iter()
                .filter(|row| row.agent_id == owner.0)
                .count()
                <= dudes_proto::MAX_CHAT_CONTEXT_PER_AGENT
        );
    }
    fill_global_pending(&mut p, &owners);
    let durable = p.projected.chat().unwrap();
    assert_eq!(durable.pending.len(), dudes_proto::MAX_CHAT_PENDING);
    let mut overlays = BTreeMap::new();
    for row in &durable.pending {
        overlays.insert(
            Lsn(row.note_lsn.parse().unwrap()),
            ChatOverlay {
                uncertain: true,
                delivery_started: false,
                reason: Some(dudes_proto::ChatReason::UnsupportedDelivery),
            },
        );
    }
    let overlaid = p.projected.chat_with_overlays(&overlays).unwrap();
    assert_eq!(overlaid.pending.len(), 128);
    assert_serialized_chat_bounds(&overlaid);
    let candidate = dudes_proto::ChatPendingView {
        agent_id: agent(),
        note_lsn: u64::MAX.to_string(),
        goal_id: Some(goal()),
        text: String::from("one too many"),
        state: ChatPendingState::Queued,
        turn_id: None,
        cancellable: true,
        reason: None,
    };
    assert!(!dudes_journal::admits_pending(&durable.pending, &candidate).unwrap());
}

#[test]
fn a_changed_goal_never_transfers_an_old_note_to_a_new_issue() {
    let mut p = Prefix::new();
    let note = p.queue("old goal only", false);
    let new_goal = GoalId::from_parts(2, 99);
    p.append(json!({"t":"goal_set","agent":agent(),"goal":{"id":new_goal,"text":"New owned goal","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":50}}));
    let pending = p.projected.chat().unwrap().pending;
    assert_eq!(pending[0].goal_id, Some(goal()));
    assert_eq!(
        pending[0].reason,
        Some(dudes_proto::ChatReason::GoalChanged)
    );
    assert!(
        p.world
            .eligible_browser_note(
                agent(),
                &dudes_proto::BrowserNoteDestination::ManagedGoal { goal: new_goal }
            )
            .is_none()
    );
    let decision = p.decide("old goal only");
    let invalid = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({
        "t":"turn_issued_with_browser_note","agent":agent(),"turn":turn(90),"goal":new_goal,"task":"old goal only",
        "session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note
    })).unwrap() };
    let before = p.projected.export().unwrap();
    let (position, crc) = p.writer.append_witnessed(&invalid).unwrap();
    assert!(p.projected.apply(position, crc, &invalid).is_err());
    assert_eq!(p.projected.export().unwrap(), before);
    p.world.apply(position, &invalid);
    assert!(!p.world.turns.contains_key(&turn(90)));
    assert_eq!(
        p.world.browser_note(note).unwrap().state,
        BrowserNoteState::Queued
    );
    let (reader, _) = Reader::open(&p.dir.path().join("journal")).unwrap();
    let mut fresh = Projections::in_memory().unwrap();
    assert!(replay(&reader, &mut fresh).is_err());
    assert_eq!(fresh.last_applied_lsn(), Lsn::ZERO);
}

const DENSE_ROWS: usize = 11;
const DENSE_ROW_BYTES: usize = 3943;

fn dense_intent_history() -> (Prefix, TurnId, Lsn) {
    let mut p = Prefix::with_fsync(dudes_journal::Fsync::None);
    let owners: Vec<_> = (2..9).map(|id| bounded_owner(&mut p, id)).collect();
    let text = "\0".repeat(DENSE_ROW_BYTES);
    for index in 0..DENSE_ROWS {
        bounded_turn(
            &mut p,
            owners[index / 2],
            7000 + u64::try_from(index).unwrap(),
            &text,
            &text,
        );
    }
    let history = p.projected.chat().unwrap().turns;
    assert_eq!(history.len(), DENSE_ROWS);
    let owner = *owners.last().unwrap();
    let turn = turn(7063);
    let empty = dudes_proto::ChatTurnView {
        reissued: false,
        agent_id: owner.0,
        turn_id: turn,
        note_lsn: p.writer.next_lsn().unwrap().0.to_string(),
        text: String::new(),
        state: ChatTurnState::Reserved,
        reply: None,
        truncated: None,
    };
    let fill = dudes_proto::MAX_CHAT_TURN_BYTES
        - serde_json::to_vec(&history).unwrap().len()
        - 1
        - serde_json::to_vec(&empty).unwrap().len();
    assert!((1..=dudes_proto::MAX_CHAT_TEXT_BYTES).contains(&fill));
    let text = "x".repeat(fill);
    let mut exact = history;
    exact.push(dudes_proto::ChatTurnView {
        text: text.clone(),
        ..empty.clone()
    });
    assert_eq!(
        serde_json::to_vec(&exact).unwrap().len(),
        dudes_proto::MAX_CHAT_TURN_BYTES
    );
    let note = bounded_queue(&mut p, owner, &text, false);
    assert_eq!(note.0.to_string(), empty.note_lsn);
    let done = p.world.completed_goal(owner.0).unwrap().1;
    let decision = p.fold(json!({"t":"note_turn_decided","agent":owner.0,"goal":owner.1,"note_lsn":note,"completion_lsn":done,"input_digest":"dense-admission","gate_trace":[],"decision":{"kind":"issue","task":text}}));
    p.fold(json!({"t":"note_only_turn_issued","agent":owner.0,"turn":turn,"goal":owner.1,"task":text,"session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note,"completion_lsn":done}));
    p.fold(json!({"t":"browser_note_dispatch_intent","agent":owner.0,"turn":turn,"note_lsn":note}));
    let agents: Vec<_> = p.world.agents.keys().copied().collect();
    for agent in agents {
        p.fold(json!({"t":"halt","agent":agent,"text":"owned replay-only fixture"}));
    }
    (p, turn, note)
}

#[test]
fn daemon_startup_reserves_uncertain_history_bytes_at_the_global_boundary() {
    let (p, turn, note) = dense_intent_history();
    let Prefix { dir, writer, .. } = p;
    drop(writer);
    let runtime = tempfile::tempdir().unwrap();
    let host = common::SharedHost::new();
    let runner = Arc::new(common::ScriptedRunner::default());
    let settings = common::settings_at(dir.path(), runtime.path());
    let daemon = duded::boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            runner.clone(),
            Arc::new(common::TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .unwrap();
    assert!(
        daemon
            .world()
            .browser_issue(turn)
            .unwrap()
            .spawn_lsn
            .is_none()
    );
    assert_eq!(
        daemon
            .world()
            .browser_issue(turn)
            .unwrap()
            .dispatch_intents
            .len(),
        1
    );
    assert!(matches!(
        daemon.world().browser_note(note).unwrap().state,
        BrowserNoteState::Reserved { .. }
    ));
    assert!(host.pids().is_empty());
    assert_eq!(runner.runs(), 0);
    drop(daemon);
    let cached = Projections::open(&dir.path().join(duded::boot::PROJECTION_FILE)).unwrap();
    let view = cached.chat().unwrap();
    let bytes = serde_json::to_vec(&view.turns).unwrap().len();
    assert!(
        bytes <= dudes_proto::MAX_CHAT_TURN_BYTES,
        "daemon startup emitted {bytes} turn bytes, above {}",
        dudes_proto::MAX_CHAT_TURN_BYTES
    );
    assert_eq!(
        view.turns.len(),
        DENSE_ROWS,
        "whole oldest history row is evicted before uncertainty can grow"
    );
    assert_eq!(view.turns.last().unwrap().turn_id, turn);
    assert_eq!(
        view.turns.last().unwrap().state,
        ChatTurnState::DeliveryUncertain
    );
    assert_eq!(view.pending.len(), 1);
    assert_eq!(view.pending[0].note_lsn, note.0.to_string());
    assert_serialized_chat_bounds(&view);
    let (reader, _) = Reader::open(&dir.path().join("journal")).unwrap();
    let mut fresh = Projections::in_memory().unwrap();
    replay(&reader, &mut fresh).unwrap();
    assert_eq!(fresh.export().unwrap(), cached.export().unwrap());
    assert_eq!(fresh.chat().unwrap(), view);
}

#[test]
fn daemon_replay_reserves_until_spawn_and_keeps_uncarried_answers() {
    let mut p = Prefix::new();
    p.append(json!({"t":"human_question","agent":agent(),"text":"carried question"}));
    let carried = p.append(json!({"t":"human_answer","agent":agent(),"text":"carried answer"}));
    p.append(json!({"t":"human_question","agent":agent(),"text":"saved question"}));
    let saved = p.append(json!({"t":"human_answer","agent":agent(),"text":"saved answer"}));
    let note = p.queue("browser note", false);
    let decision = p.decide("carried answer and browser note");
    p.issue(
        turn(80),
        note,
        decision,
        None,
        "carried answer and browser note",
        &[carried],
    );
    p.append(json!({"t":"hook_token_minted","agent":agent(),"token_hash":"11".repeat(32)}));
    let reserved = p.daemon_replayed().unwrap();
    assert!(matches!(
        reserved.world.browser_note(note).unwrap().state,
        BrowserNoteState::Reserved { .. }
    ));
    assert_eq!(reserved.projected.chat().unwrap().pending.len(), 1);
    assert_eq!(reserved.world.agents[&agent()].pending_answers.len(), 2);
    drop(reserved);
    p.intent(turn(80), note);
    p.spawn(turn(80));
    let spawned = p.daemon_replayed().unwrap();
    assert!(matches!(
        spawned.world.browser_note(note).unwrap().state,
        BrowserNoteState::Delivered { .. }
    ));
    assert!(spawned.projected.chat().unwrap().pending.is_empty());
    assert_eq!(spawned.world.agents[&agent()].pending_answers.len(), 1);
    assert_eq!(spawned.world.agents[&agent()].pending_answers[0].lsn, saved);
}

#[test]
fn daemon_replay_consumes_the_exact_note_and_clean_cancellation_keeps_facts() {
    let mut p = Prefix::new();
    p.append(json!({"t":"attached","agent":agent(),"by":{"kind":"system"}}));
    let old = p.queue("older attached context", true);
    p.append(json!({"t":"detached","agent":agent(),"by":{"kind":"system"}}));
    let note = p.queue("current managed note", false);
    let decision = p.decide("current managed note");
    p.issue(turn(81), note, decision, None, "current managed note", &[]);
    p.intent(turn(81), note);
    p.spawn(turn(81));
    let spawned = p.daemon_replayed().unwrap();
    assert_eq!(
        spawned.world.browser_note(old).unwrap().state,
        BrowserNoteState::Queued
    );
    assert!(matches!(
        spawned.world.browser_note(note).unwrap().state,
        BrowserNoteState::Delivered { .. }
    ));
    drop(spawned);
    let mut p = Prefix::new();
    let clean = p.queue("never dispatched", false);
    let decision = p.decide("never dispatched");
    p.issue(turn(82), clean, decision, None, "never dispatched", &[]);
    let end_provenance = p.world.agents[&agent()].last_turn_end;
    p.append(json!({"t":"browser_note_issued_cancelled","agent":agent(),"note_lsn":clean,"turn":turn(82),"browser":p.attribution()}));
    let cancelled = p.daemon_replayed().unwrap();
    assert_eq!(cancelled.world.goals[&goal()].turns_on_goal, 1);
    assert_eq!(
        cancelled.world.agents[&agent()].last_turn_end,
        end_provenance
    );
    assert!(cancelled.world.browser_issue_cancelled(turn(82)));
    assert!(cancelled.projected.open_turns().unwrap().is_empty());
}

#[test]
fn daemon_replay_failed_launch_stays_terminal_and_admission_is_not_goal_done() {
    let mut p = Prefix::new();
    let note = p.queue("will not launch", false);
    let done = p.complete();
    let decision = p.note_decide(note, done, "will not launch");
    p.issue(turn(83), note, decision, Some(done), "will not launch", &[]);
    p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(83),"mode":"managed","provenance":"observed",
        "exit":{"code":null},"outcome":{"kind":"not_launched","reason":"owned failure"},"last_message":null,"usage":{}}));
    let failed = p.daemon_replayed().unwrap();
    assert!(matches!(
        failed.world.browser_note(note).unwrap().state,
        BrowserNoteState::Failed { .. }
    ));
    assert!(
        failed
            .world
            .eligible_browser_note(
                agent(),
                &dudes_proto::BrowserNoteDestination::ManagedGoal { goal: goal() }
            )
            .is_none()
    );
    assert!(failed.projected.chat().unwrap().pending.is_empty());
    assert_eq!(
        failed.projected.chat().unwrap().turns[0].state,
        ChatTurnState::Failed
    );
    assert_eq!(failed.world.completed_goal(agent()), Some((goal(), done)));
    let activity = serde_json::to_value(failed.projected.activity().unwrap()).unwrap();
    assert_eq!(
        activity
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "goal_done")
            .count(),
        1
    );
}

#[test]
fn daemon_replay_replies_keep_exact_join_and_nullable_completeness() {
    let mut p = Prefix::new();
    let note = p.queue("reply owner", false);
    let decision = p.decide("reply owner");
    p.issue(turn(84), note, decision, None, "reply owner", &[]);
    p.intent(turn(84), note);
    p.spawn(turn(84));
    p.end(turn(84), Some(&format!("{}...", "x".repeat(4093))));
    let decision = p.decide("unrelated work");
    p.append(json!({"t":"turn_issued","agent":agent(),"turn":turn(85),"goal":goal(),"task":"unrelated work","session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[]}));
    p.spawn(turn(85));
    p.end(turn(85), Some("unrelated final sentinel"));
    let ordinary = p.daemon_replayed().unwrap();
    let view = ordinary.projected.chat().unwrap();
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].reply.as_ref().unwrap().len(), 4096);
    assert_eq!(view.turns[0].truncated, None);
    assert!(!serde_json::to_string(&view).unwrap().contains("sentinel"));
    drop(ordinary);
    p.append(json!({"t":"attached","agent":agent(),"by":{"kind":"system"}}));
    let context = p.queue("context only", true);
    p.append(json!({"t":"browser_note_context_emitted","agent":agent(),"note_lsn":context}));
    p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(86),"mode":"attached","provenance":"observed",
        "exit":{"code":0},"outcome":{"kind":"completed"},"last_message":"attached final sentinel","usage":{}}));
    let attached = p.daemon_replayed().unwrap();
    let view = attached.projected.chat().unwrap();
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].reply.as_ref().unwrap().len(), 4096);
    assert_eq!(view.context_sent.len(), 1);
    assert!(!serde_json::to_string(&view).unwrap().contains("sentinel"));
}

#[test]
fn daemon_boot_refuses_a_second_issue_for_one_note() {
    let mut p = Prefix::new();
    let note = p.queue("once", false);
    let decision = p.decide("once");
    p.issue(turn(87), note, decision, None, "once", &[]);
    p.intent(turn(87), note);
    p.spawn(turn(87));
    p.end(turn(87), None);
    let decision = p.decide("again");
    p.writer.append(&Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({
        "t":"turn_issued_with_browser_note","agent":agent(),"turn":turn(88),"goal":goal(),"task":"again","session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note
    })).unwrap() }).unwrap();
    assert!(
        p.daemon_replayed().is_err(),
        "actual boot must refuse the duplicate note join"
    );
}

#[test]
fn daemon_boot_refuses_old_goal_note_transfer() {
    let mut p = Prefix::new();
    let note = p.queue("old goal", false);
    let new_goal = GoalId::from_parts(2, 101);
    p.append(json!({"t":"goal_set","agent":agent(),"goal":{"id":new_goal,"text":"new goal","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":50}}));
    let decision = p.decide("old goal");
    p.writer.append(&Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({
        "t":"turn_issued_with_browser_note","agent":agent(),"turn":turn(89),"goal":new_goal,"task":"old goal","session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[],"note_lsn":note
    })).unwrap() }).unwrap();
    assert!(
        p.daemon_replayed().is_err(),
        "actual boot must refuse the old-goal note join"
    );
}

fn daemon_refuses_framing(bytes: &[u8], expected: &str) {
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let journal = state.path().join("journal");
    std::fs::create_dir(&journal).unwrap();
    let segment = dudes_journal::segment::segment_path(&journal, 1);
    std::fs::write(&segment, bytes).unwrap();
    let host = common::SharedHost::new();
    let settings = common::settings_at(state.path(), runtime.path());
    let result = duded::boot(
        &settings,
        duded::Seams {
            host: host.clone(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(common::ScriptedRunner::default()),
            clock: Arc::new(common::TestClock::new()),
            rng: dudes_supervisor::testing::FakeRng::new(),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: dudes_providers::registry::Adapters::new([]),
            children: duded::ChildEnds::new(),
            witness: duded::startup::Witness::default(),
        },
    );
    let error = match result {
        Err(error) => format!("{error:#}"),
        Ok(daemon) => {
            drop(daemon);
            panic!("actual daemon boot accepted forbidden framing")
        }
    };
    assert!(error.contains(expected), "{error}");
    assert_eq!(std::fs::read(segment).unwrap(), bytes);
    assert!(!state.path().join(duded::boot::PROJECTION_FILE).exists());
    assert!(
        !runtime
            .path()
            .join("run")
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME)
            .exists()
    );
    assert!(host.spawned().is_empty());
}

fn framing(version: u16, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = dudes_journal::frame::Header {
        len: u32::try_from(payload.len()).unwrap(),
        ver: version,
        kind,
        crc: dudes_journal::frame::crc32c(version, kind, payload),
    }
    .to_bytes()
    .to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn daemon_boot_refuses_unknown_v2_before_cache_or_log_mutation() {
    daemon_refuses_framing(
        &framing(2, 1000, b"unknown required"),
        "required record kind 1000 at 1:0 is not understood",
    );
}

#[test]
fn daemon_boot_refuses_v2_after_a_damaged_length_before_repair() {
    let resume = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: dudes_proto::Record::Resume {
            agent: agent(),
            browser: None,
        },
    };
    let payload = serde_json::to_vec(&resume).unwrap();
    let mut bytes = framing(1, dudes_proto::RecordKind::Resume.tag(), &payload);
    bytes[..4].copy_from_slice(&16_777_216u32.to_le_bytes());
    bytes.extend(framing(2, dudes_proto::RecordKind::Resume.tag(), &payload));
    daemon_refuses_framing(&bytes, "short frame: 16777228 bytes needed");
}

fn conversation_prefix() -> (Prefix, Lsn, Lsn, Lsn) {
    conversation_prefix_after(Prefix::new())
}

fn conversation_prefix_after(mut p: Prefix) -> (Prefix, Lsn, Lsn, Lsn) {
    let context = p.append(json!({"t":"goal_cleared","agent":agent(),"goal":goal()}));
    let note = p.append(json!({"t":"conversation_message_queued","agent":agent(),"text":"Hello","browser":p.attribution(),"context_lsn":context,"charge_scope":"agent_and_floor"}));
    let decision = p.append(json!({"t":"conversation_turn_decided","agent":agent(),"note_lsn":note,"context_lsn":context,"charge_scope":"agent_and_floor","input_digest":"fixture admission","gate_trace":[],"decision":{"kind":"issue","task":"Hello"}}));
    (p, context, note, decision)
}

fn conversation_issue(context: Lsn, note: Lsn, decision: Lsn) -> Value {
    json!({"t":"conversation_turn_issued","agent":agent(),"turn":turn(120),"note_lsn":note,"context_lsn":context,"charge_scope":"agent_and_floor","task":"Hello","session":null,"model_selection":{},"lsn_of_decision":decision})
}

#[test]
fn conversation_prefixes_keep_one_owner_and_no_goal_charge_after_goal_changes() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    assert_eq!(p.world.turns[&turn(120)].goal, None);
    assert_eq!(p.projected.turn(turn(120)).unwrap().unwrap().goal, None);
    let new_goal = GoalId::from_parts(2, 30);
    p.append(json!({"t":"goal_set","agent":agent(),"goal":{"id":new_goal,"text":"Later task","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":1}}));
    p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(120),"mode":"managed","provenance":"observed","exit":{"code":0,"signal":null},"outcome":{"kind":"completed"},"final_reply":{"text":"Hello back","completeness":"complete"}}));
    let chat = p.projected.chat().unwrap();
    assert_eq!(chat.turns[0].reply.as_deref(), Some("Hello back"));
    assert_eq!(chat.turns[0].truncated, Some(false));
    let spend = p
        .projected
        .spend_views(agent(), new_goal, Timestamp::UNIX_EPOCH.utc_day())
        .unwrap();
    assert_eq!(spend.goal_unknown_ends, 0);
    assert_eq!(spend.agent_unknown_ends_today, 1);
    assert_eq!(spend.floor_unknown_ends_today, 1);
}

#[test]
fn conversation_issue_rejects_mismatched_authority_and_issue_cancellation() {
    for field in ["note_lsn", "context_lsn", "lsn_of_decision", "task"] {
        let (mut p, context, note, decision) = conversation_prefix();
        let mut bad = conversation_issue(context, note, decision);
        bad[field] = if field == "task" {
            json!("other input")
        } else {
            json!(0)
        };
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: serde_json::from_value(bad).unwrap(),
        };
        let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
        assert!(p.projected.apply(lsn, crc, &entry).is_err(), "{field}");
        p.world.apply(lsn, &entry);
        assert!(!p.world.turns.contains_key(&turn(120)), "{field}");
    }
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({"t":"browser_note_issued_cancelled","agent":agent(),"note_lsn":note,"turn":turn(120),"browser":p.attribution()})).unwrap() };
    let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
    assert!(p.projected.apply(lsn, crc, &entry).is_err());
    p.world.apply(lsn, &entry);
    assert!(!p.world.browser_issue_cancelled(turn(120)));
    assert_eq!(p.world.agents[&agent()].open_turn, Some(turn(120)));
}

#[test]
fn conversation_long_final_replies_remain_visible_with_honest_serialized_truncation() {
    for text in ["é".repeat(32_768), "\u{0001}".repeat(65_536)] {
        let (mut p, context, note, decision) = conversation_prefix();
        p.append(conversation_issue(context, note, decision));
        p.append(json!({"t":"turn_ended","agent":agent(),"turn":turn(120),"mode":"managed","provenance":"observed","exit":{"code":0,"signal":null},"outcome":{"kind":"completed"},"final_reply":{"text":text,"completeness":"complete"}}));
        let chat = p.projected.chat().unwrap();
        assert_eq!(chat.turns.len(), 1);
        let retained = chat.turns[0].reply.as_ref().unwrap();
        assert!(!retained.is_empty());
        assert!(text.starts_with(retained));
        assert_eq!(chat.turns[0].truncated, Some(retained.len() < text.len()));
        assert!(
            serde_json::to_vec(&chat.turns).unwrap().len()
                <= dudes_proto::MAX_CHAT_TURN_BYTES_PER_AGENT
        );
        assert!(serde_json::to_vec(&chat).unwrap().len() <= dudes_proto::MAX_CHAT_BYTES);
    }
}

#[test]
fn conversation_rejects_two_final_reply_sources_without_ending_or_charging() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({"t":"turn_ended","agent":agent(),"turn":turn(120),"mode":"managed","provenance":"observed","last_message":"legacy text","final_reply":{"text":"new text","completeness":"complete"}})).unwrap() };
    let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
    assert!(p.projected.apply(lsn, crc, &entry).is_err());
    p.world.apply(lsn, &entry);
    assert_eq!(p.world.agents[&agent()].open_turn, Some(turn(120)));
    assert!(p.projected.chat().unwrap().turns[0].reply.is_none());
}

#[test]
fn conversation_issue_cannot_replace_an_ended_ordinary_turn_from_any_owner() {
    for owner in [agent(), AgentId::from_parts(1, 45)] {
        let mut p = Prefix::new();
        let original_goal = if owner == agent() {
            goal()
        } else {
            p.append(json!({"t":"agent_hired","id":owner,"name":"Other owner","role":"test","department":"test","provider":"claude","cwd":p.dir.path(),"budget":{}}));
            let assigned = GoalId::from_parts(2, 45);
            p.append(json!({"t":"goal_set","agent":owner,"goal":{"id":assigned,"text":"Other work","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":5}}));
            assigned
        };
        let decision = p.append(json!({"t":"loop_decided","agent":owner,"input_digest":"old admission","decision":{"kind":"continue","task":"old ordinary task"},"gate_trace":[]}));
        p.append(json!({"t":"turn_issued","agent":owner,"turn":turn(120),"goal":original_goal,"task":"old ordinary task","session":null,"mode":"managed","model_selection":{},"lsn_of_decision":decision,"answers":[]}));
        p.append(json!({"t":"turn_ended","agent":owner,"turn":turn(120),"mode":"managed","provenance":"observed","usage":{},"outcome":{"kind":"completed"}}));
        let (mut p, context, note, decision) = conversation_prefix_after(p);
        let before = p.projected.export().unwrap();
        let original = p.projected.turn(turn(120)).unwrap().unwrap();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: serde_json::from_value(conversation_issue(context, note, decision)).unwrap(),
        };
        let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
        assert!(p.projected.apply(lsn, crc, &entry).is_err());
        assert_eq!(p.projected.export().unwrap(), before);
        assert_eq!(p.projected.turn(turn(120)).unwrap().unwrap(), original);
        p.world.apply(lsn, &entry);
        assert_eq!(p.world.turns[&turn(120)].agent, owner);
        assert!(p.world.turns[&turn(120)].has_ended());
        assert_eq!(
            p.world.browser_note(note).unwrap().state,
            BrowserNoteState::Queued
        );
    }
}

fn conversation_question_prefix() -> (Prefix, Lsn, Lsn) {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    p.intent(turn(120), note);
    p.spawn(turn(120));
    let question = p.append(json!({"t":"conversation_question","agent":agent(),"origin":{"turn":turn(120),"context_lsn":context},"text":"Which branch?"}));
    p.end(turn(120), None);
    (p, context, question)
}

#[test]
fn conversation_answer_replay_closes_only_the_exact_origin_and_queues_once() {
    for field in ["question_lsn", "origin", "task"] {
        let (mut p, context, question) = conversation_question_prefix();
        let mut answer = json!({"t":"conversation_answer_queued","agent":agent(),"question_lsn":question,"origin":{"turn":turn(120),"context_lsn":context},"text":"main","task":dudes_proto::conversation_answer_task("Which branch?", "main"),"authority":{"kind":"operator","provenance":null},"charge_scope":"agent_and_floor"});
        answer[field] = match field {
            "question_lsn" => json!(0),
            "origin" => json!({"turn":turn(121),"context_lsn":context}),
            _ => json!("different task"),
        };
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: serde_json::from_value(answer).unwrap(),
        };
        let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
        let before = p.projected.export().unwrap();
        assert!(p.projected.apply(lsn, crc, &entry).is_err());
        assert_eq!(p.projected.export().unwrap(), before);
        p.world.apply(lsn, &entry);
        assert_eq!(p.world.agents[&agent()].question_lsn, Some(question));
        assert!(p.world.browser_note(lsn).is_none());
    }
    let (mut p, context, question) = conversation_question_prefix();
    let answer = p.append(json!({"t":"conversation_answer_queued","agent":agent(),"question_lsn":question,"origin":{"turn":turn(120),"context_lsn":context},"text":"main","task":dudes_proto::conversation_answer_task("Which branch?", "main"),"authority":{"kind":"operator","provenance":null},"charge_scope":"agent_and_floor"}));
    assert_eq!(p.world.agents[&agent()].question_lsn, None);
    assert_eq!(
        p.projected.agent(agent()).unwrap().unwrap().question_lsn,
        None
    );
    assert_eq!(p.projected.chat().unwrap().pending[0].text, "main");
    assert_eq!(
        p.world.browser_note(answer).unwrap().answer_to,
        Some(question)
    );
}

#[test]
fn conversation_answer_reservation_rejects_unrelated_issuance_at_replay() {
    let (mut p, context, question) = conversation_question_prefix();
    let unrelated = p.append(json!({"t":"conversation_message_queued","agent":agent(),"text":"later unrelated","browser":p.attribution(),"context_lsn":context,"charge_scope":"agent_and_floor"}));
    p.append(json!({"t":"conversation_answer_queued","agent":agent(),"question_lsn":question,"origin":{"turn":turn(120),"context_lsn":context},"text":"main","task":dudes_proto::conversation_answer_task("Which branch?", "main"),"authority":{"kind":"operator","provenance":null},"charge_scope":"agent_and_floor"}));
    let decision = p.append(json!({"t":"conversation_turn_decided","agent":agent(),"note_lsn":unrelated,"context_lsn":context,"charge_scope":"agent_and_floor","input_digest":"unrelated admission","gate_trace":[],"decision":{"kind":"issue","task":"later unrelated"}}));
    let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({"t":"conversation_turn_issued","agent":agent(),"turn":turn(121),"note_lsn":unrelated,"context_lsn":context,"charge_scope":"agent_and_floor","task":"later unrelated","session":null,"model_selection":{},"lsn_of_decision":decision})).unwrap() };
    let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
    assert!(p.projected.apply(lsn, crc, &entry).is_err());
    p.world.apply(lsn, &entry);
    assert!(!p.world.turns.contains_key(&turn(121)));
}

#[test]
fn conversation_retry_reserve_is_per_dispatch_and_survives_a_known_retry_end() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let prior = p.intent(turn(120), note);
    let stale = p.append(json!({"t":"conversation_message_queued","agent":agent(),"text":"older queued input","browser":p.attribution(),"context_lsn":context,"charge_scope":"agent_and_floor"}));
    p.append(json!({"t":"conversation_dispatch_uncertain","agent":agent(),"turn":turn(120),"dispatch_intent_lsn":prior,"charge_scope":"agent_and_floor"}));
    p.intent(turn(120), note);
    p.append(json!({"t":"turn_spawned","agent":agent(),"turn":turn(120),"pid":4242,"start_time":1,"session":null,"reissued":true}));
    p.end(turn(120), Some("known retry reply"));
    let spend = p
        .projected
        .conversation_spend_views(agent(), Timestamp::UNIX_EPOCH.utc_day())
        .unwrap();
    assert_eq!(spend.agent_unknown_ends_today, 1);
    assert_eq!(spend.floor_unknown_ends_today, 1);
    assert_eq!(spend.goal_unknown_ends, 0);
    let view = p.projected.chat().unwrap();
    assert!(view.turns[0].reissued);
    let pending = view
        .pending
        .iter()
        .find(|row| row.note_lsn == stale.0.to_string())
        .unwrap();
    assert_eq!(
        pending.reason,
        Some(dudes_proto::ChatReason::ContextChanged)
    );
    assert!(pending.cancellable);
    assert_eq!(
        p.world
            .browser_issue(turn(120))
            .unwrap()
            .uncertain_dispatches,
        [prior]
    );
}

#[test]
fn conversation_retry_reserve_refuses_duplicate_or_unmatched_dispatch() {
    for duplicate in [false, true] {
        let (mut p, context, note, decision) = conversation_prefix();
        p.append(conversation_issue(context, note, decision));
        let prior = p.intent(turn(120), note);
        if duplicate {
            p.append(json!({"t":"conversation_dispatch_uncertain","agent":agent(),"turn":turn(120),"dispatch_intent_lsn":prior,"charge_scope":"agent_and_floor"}));
        }
        let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({"t":"conversation_dispatch_uncertain","agent":agent(),"turn":turn(120),"dispatch_intent_lsn":if duplicate {prior} else {Lsn::ZERO},"charge_scope":"agent_and_floor"})).unwrap() };
        let before = p.projected.export().unwrap();
        let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
        assert!(p.projected.apply(lsn, crc, &entry).is_err());
        assert_eq!(p.projected.export().unwrap(), before);
    }
}

#[test]
fn startup_reserves_uncertain_conversation_before_all_admission() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let prior = p.intent(turn(120), note);
    let other = AgentId::from_parts(4, 1);
    p.append(json!({"t":"agent_hired","id":other,"name":"Other agent","role":"test","department":"test","provider":"claude","cwd":p.dir.path(),"budget":{}}));
    p.append(json!({"t":"goal_set","agent":other,"goal":{"id":GoalId::from_parts(4,2),"text":"must wait for compliance","predicate":{"command":{"argv":["/usr/bin/true"],"timeout_s":1}},"budget":{},"hop_cap":5}}));
    let Prefix { dir, writer, .. } = p;
    drop(writer);
    let runtime = tempfile::tempdir().unwrap();
    let mut settings = common::settings_at(dir.path(), runtime.path());
    settings.floor_usd_per_day = Some(dudes_types::Usd::from_micros(1));
    for _ in 0..2 {
        let host = common::SharedHost::new();
        let clock = Arc::new(common::TestClock::new());
        let mut seams = common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::clone(&clock),
            duded::startup::Witness::default(),
        );
        seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: dir.path().join("missing-fixture-provider").into_os_string(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect();
        let daemon =
            duded::boot_with_options(&settings, seams, duded::settings::RunOptions::default())
                .unwrap();
        assert_eq!(
            daemon
                .world()
                .browser_issue(turn(120))
                .unwrap()
                .uncertain_dispatches,
            [prior]
        );
        assert!(host.spawned().is_empty());
        drop(daemon);
        let projected = Projections::open(&dir.path().join(duded::boot::PROJECTION_FILE)).unwrap();
        let day = dudes_supervisor::Clock::now(clock.as_ref()).utc_day();
        let spend = projected.conversation_spend_views(agent(), day).unwrap();
        assert_eq!(spend.agent_unknown_ends_today, 1);
        assert_eq!(spend.floor_unknown_ends_today, 1);
        assert_eq!(
            projected.agent(other).unwrap().unwrap().status,
            dudes_types::AgentStatus::Exhausted
        );
    }
}

#[test]
fn conversation_issued_under_previous_credentials_stays_owned_when_replay_needs_a_ceiling() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let Prefix { dir, writer, .. } = p;
    drop(writer);
    let runtime = tempfile::tempdir().unwrap();
    let mut settings = common::settings_at(dir.path(), runtime.path());
    settings.floor_usd_per_day = None;
    settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "ANTHROPIC_API_KEY",
            "owned-fixture-marker",
        ));
    let host = common::SharedHost::new();
    let daemon = duded::boot_with_options(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            duded::startup::Witness::default(),
        ),
        duded::settings::RunOptions::default(),
    )
    .unwrap();
    assert!(host.spawned().is_empty());
    assert!(daemon.world().turns[&turn(120)].is_issued_not_spawned());
    assert!(!daemon.world().browser_issue_cancelled(turn(120)));
    assert_eq!(daemon.world().agents[&agent()].open_turn, Some(turn(120)));
    drop(daemon);
    settings
        .daemon_env
        .retain(|entry| entry.name() != std::ffi::OsStr::new("ANTHROPIC_API_KEY"));
    let host = common::SharedHost::new();
    let daemon = duded::boot_with_options(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            duded::startup::Witness::default(),
        ),
        duded::settings::RunOptions::default(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let records = loop {
        let (reader, _) = Reader::open(&dir.path().join("journal")).unwrap();
        let records: Vec<_> = reader
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(|entry| entry.unwrap().1.record)
            .collect();
        if records.iter().any(|record| matches!(record, dudes_proto::Record::TurnSpawned { turn: id, reissued: true, .. } if *id == turn(120))) { break records; }
        assert!(
            std::time::Instant::now() < deadline,
            "restored billing configuration must release the existing issue"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, dudes_proto::Record::ConversationTurnIssued { .. }))
            .count(),
        1
    );
    assert_eq!(host.spawned().len(), 1);
    running.stop(duded::ExitCause::Signal);
}

#[test]
fn conversation_question_after_reissue_binds_the_recovered_context() {
    let (mut p, context, note, decision) = conversation_prefix();
    p.append(conversation_issue(context, note, decision));
    let prior = p.intent(turn(120), note);
    p.append(json!({"t":"conversation_dispatch_uncertain","agent":agent(),"turn":turn(120),"dispatch_intent_lsn":prior,"charge_scope":"agent_and_floor"}));
    p.intent(turn(120), note);
    let recovered = p.append(json!({"t":"turn_spawned","agent":agent(),"turn":turn(120),"pid":4242,"start_time":1,"session":null,"reissued":true}));
    let origin = json!({"turn":turn(120),"context_lsn":recovered});
    let question = p.append(
        json!({"t":"conversation_question","agent":agent(),"origin":origin,"text":"Which branch?"}),
    );
    p.end(turn(120), None);
    let task = dudes_proto::conversation_answer_task("Which branch?", "main");
    let answer = p.append(json!({"t":"conversation_answer_queued","agent":agent(),"question_lsn":question,"origin":origin,"text":"main","task":task,"authority":{"kind":"operator","provenance":null},"charge_scope":"agent_and_floor"}));
    let decision = p.append(json!({"t":"conversation_turn_decided","agent":agent(),"note_lsn":answer,"context_lsn":recovered,"charge_scope":"agent_and_floor","input_digest":"recovered answer admission","gate_trace":[],"decision":{"kind":"issue","task":task}}));
    p.append(json!({"t":"conversation_turn_issued","agent":agent(),"turn":turn(121),"note_lsn":answer,"context_lsn":recovered,"charge_scope":"agent_and_floor","task":task,"session":null,"model_selection":{},"lsn_of_decision":decision}));
    assert_eq!(p.world.agents[&agent()].open_turn, Some(turn(121)));
    assert_eq!(
        p.projected.agent(agent()).unwrap().unwrap().open_turn,
        Some(turn(121))
    );
}

#[test]
fn conversation_ordinary_fifo_mismatch_refuses_transactionally() {
    let (mut p, context, first, _) = conversation_prefix();
    let second = p.append(json!({"t":"conversation_message_queued","agent":agent(),"text":"second","browser":p.attribution(),"context_lsn":context,"charge_scope":"agent_and_floor"}));
    let decision = p.append(json!({"t":"conversation_turn_decided","agent":agent(),"note_lsn":second,"context_lsn":context,"charge_scope":"agent_and_floor","input_digest":"out of order","gate_trace":[],"decision":{"kind":"issue","task":"second"}}));
    let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(json!({"t":"conversation_turn_issued","agent":agent(),"turn":turn(120),"note_lsn":second,"context_lsn":context,"charge_scope":"agent_and_floor","task":"second","session":null,"model_selection":{},"lsn_of_decision":decision})).unwrap() };
    let before = p.projected.export().unwrap();
    let (lsn, crc) = p.writer.append_witnessed(&entry).unwrap();
    assert!(p.projected.apply(lsn, crc, &entry).is_err());
    assert_eq!(p.projected.export().unwrap(), before);
    p.world.apply(lsn, &entry);
    assert!(!p.world.turns.contains_key(&turn(120)));
    assert_eq!(
        p.world.browser_note(first).unwrap().state,
        BrowserNoteState::Queued
    );
    assert_eq!(
        p.world.browser_note(second).unwrap().state,
        BrowserNoteState::Queued
    );
    let (reader, _) = Reader::open(&p.dir.path().join("journal")).unwrap();
    assert!(replay(&reader, &mut Projections::in_memory().unwrap()).is_err());
}
