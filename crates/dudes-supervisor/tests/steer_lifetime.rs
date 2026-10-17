//! A steer is consumed by its first delivery.
//!
//! The defect: `Record::Steer` set a field that nothing ever cleared, so every reply-bearing hook
//! event re-attached it — an agent steered once was told the same thing at every boundary for the
//! rest of its life. The fix is a queue whose notes are named by the position they were journaled
//! at, delivered at two event kinds only, and removed by a record.
//!
//! **On positions in this file.** A note's identity is the `Lsn` of the `Record::Steer` that
//! queued it. Nothing here decodes one, orders by one, or compares one against a position from
//! another log: every assertion reads the position out of the same `World` that produced it and
//! checks equality. That is what keeps the tests honest about a real journal, whose first segment
//! is 1 (`dudes_journal::FIRST_SEGMENT`) while the harness mints its acknowledgements in segment
//! 0 — a coordinate no writer produces. [`Hand`], which mints its own, uses segment 1 for the
//! same reason `dudes-proto`'s samples do.

use std::collections::VecDeque;

use dudes_proto::{Entry, Event, EventKind, HookDecision, HookReply, PeerProvenance, Record};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, HookId, Input, PersistToken};
use dudes_types::{AgentId, Lsn, Provenance, Timestamp};

fn agent_id() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id")
}

fn steer(agent: AgentId, text: &str) -> Command {
    Command::Steer {
        agent,
        text: text.to_owned(),
        provenance: Some(PeerProvenance::NoRecordedAgent),
    }
}

fn event(agent: AgentId, kind: EventKind) -> Event {
    Event {
        agent,
        at: Timestamp::from_unix_millis(1),
        provenance: Provenance::Observed,
        kind,
        raw: None,
    }
}

/// Drive one hook to quiescence and return the context its reply carried.
fn reply_to(
    harness: &mut Harness,
    agent: AgentId,
    hook: HookId,
    kind: EventKind,
) -> Option<String> {
    reply(harness, agent, hook, kind).context
}

fn reply(harness: &mut Harness, agent: AgentId, hook: HookId, kind: EventKind) -> HookReply {
    harness.drive(Input::Hook {
        hook,
        event: event(agent, kind),
    });
    harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Reply { hook: id, reply } if *id == hook => Some(reply.clone()),
            _ => None,
        })
        .expect("the hook is answered")
}

/// The agent's pending notes, oldest first.
fn pending(harness: &Harness, agent: AgentId) -> Vec<String> {
    harness.supervisor.world().agents[&agent]
        .steers
        .iter()
        .map(|(_, text)| text.clone())
        .collect()
}

/// The positions those notes are named by, in the same order.
fn positions(harness: &Harness, agent: AgentId) -> Vec<Lsn> {
    harness.supervisor.world().agents[&agent]
        .steers
        .iter()
        .map(|(lsn, _)| *lsn)
        .collect()
}

/// Every `SteerConsumed` the machine asked to have written, in order.
fn consumed(harness: &Harness) -> Vec<Lsn> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::SteerConsumed { steer_lsn, .. } => Some(*steer_lsn),
            _ => None,
        })
        .collect()
}

/// Every `SteerCleared` the machine asked to have written, in order.
fn cleared(harness: &Harness) -> Vec<Lsn> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::SteerCleared { steer_lsn, .. } => Some(*steer_lsn),
            _ => None,
        })
        .collect()
}

// ---- the two admitted kinds, and the boundary that is not one ---------------------------------

/// A stop boundary never carries a steer, and does not spend one either.
///
/// This is where the compounding happened: composed with the stop cell, a steer-carrying `Allow` at
/// a stop boundary lands in the vendor's `continue` cell, so an agent steered once was told to
/// keep working every time it tried to stop. Both boundary kinds are driven, because
/// `SubagentEnd` is governed identically and a fix that caught only `TurnEnd` would leave half
/// the defect standing.
#[test]
fn a_stop_boundary_never_carries_a_steer_and_never_spends_one() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(steer(agent, "rebase before you push")));

    assert_eq!(
        reply_to(
            &mut harness,
            agent,
            HookId(1),
            EventKind::TurnEnd { last_message: None }
        ),
        None,
        "a turn end is a stop boundary, not a correction point"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(2), EventKind::SubagentEnd),
        None,
        "a subagent end is the same boundary"
    );
    assert_eq!(
        pending(&harness, agent),
        vec!["rebase before you push".to_owned()],
        "neither boundary spent the note: it is still there for the next admitted event"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(3), EventKind::PromptSubmit),
        Some("rebase before you push".to_owned()),
        "and the next admitted event delivers it"
    );
}

/// `PostTool` is admitted with no predicate on `ok`.
#[test]
fn a_post_tool_event_carries_a_steer_whether_or_not_the_tool_succeeded() {
    for ok in [true, false] {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.drive(Input::Command(steer(agent, "check the exit status")));

        assert_eq!(
            reply_to(
                &mut harness,
                agent,
                HookId(4),
                EventKind::PostTool {
                    name: "Bash".to_owned(),
                    ok,
                }
            ),
            Some("check the exit status".to_owned()),
            "a post-tool event is a correction point regardless of ok={ok}"
        );
    }
}

// ---- one note per reply, oldest first ---------------------------------------------------------

/// Two corrections arrive **in order**, one per event, never merged.
///
/// The failing case for the removal logic: a fold that popped the front instead of removing the
/// note at `steer_lsn` passes this test, so the third arm drives an inadmissible event between
/// the two deliveries — under a pop-the-front fold the queue is unchanged there, but under a
/// remove-by-position fold so is it. What separates them is
/// [`a_consume_removes_the_note_it_names_and_not_the_front`] below.
#[test]
fn two_notes_arrive_oldest_first_and_one_per_event() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(steer(agent, "first: rebase")));
    harness.drive(Input::Command(steer(agent, "second: squash")));

    assert_eq!(
        reply_to(&mut harness, agent, HookId(5), EventKind::PromptSubmit),
        Some("first: rebase".to_owned()),
        "the oldest note goes first"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(6), EventKind::PromptSubmit),
        Some("second: squash".to_owned()),
        "the next event carries the next note, not a concatenation of both"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(7), EventKind::PromptSubmit),
        None,
        "and the queue is empty"
    );
}

/// The consume names **the position of the note it delivered**, and the fold removes that one.
///
/// A live-path pin, and deliberately not the discriminating one: on this path the delivered note
/// is always the oldest, so `pop_front()` and remove-by-position agree here and this test passes
/// under both. What separates them is
/// [`a_removal_takes_the_note_it_names_and_a_repeat_or_stranger_takes_nothing`], which folds a
/// journal whose removal is not the front. This test sets up no paused-agent ordering, and a doc that
/// oversells a test is how a weak assertion survives review.
#[test]
fn a_consume_names_the_position_of_the_note_it_delivered() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(steer(agent, "first: rebase")));
    harness.drive(Input::Command(steer(agent, "second: squash")));
    let [first, second] = positions(&harness, agent)[..] else {
        panic!("two notes are pending");
    };
    assert_ne!(first, second, "each note is named by its own position");

    harness.drive(Input::Hook {
        hook: HookId(8),
        event: event(agent, EventKind::PromptSubmit),
    });

    assert_eq!(
        harness
            .records()
            .filter_map(|record| match record {
                Record::SteerConsumed { steer_lsn, .. } => Some(*steer_lsn),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![first],
        "the consume names the position of the note that was delivered"
    );
    assert_eq!(
        positions(&harness, agent),
        vec![second],
        "and the fold removed that position, leaving the other note untouched"
    );
}

// ---- the bounded queue, and the record a drop leaves ------------------------------------------

/// A twenty-first note drops the oldest, and the drop is a record.
///
/// The count and the record are asserted together on purpose: a cap that dropped silently would
/// satisfy the count alone, and that silence would be a log line, which Dudes
/// cannot rely on, because a drop changes what the fold holds (invariant 1).
#[test]
fn a_twenty_first_note_drops_the_oldest_and_records_the_drop() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    for n in 0..20 {
        harness.drive(Input::Command(steer(agent, &format!("note {n}"))));
    }
    let oldest = positions(&harness, agent)[0];
    assert_eq!(pending(&harness, agent).len(), 20, "the queue is full");
    assert!(cleared(&harness).is_empty(), "control: nothing dropped yet");

    harness.drive(Input::Command(steer(agent, "note 20")));

    assert_eq!(
        cleared(&harness),
        vec![oldest],
        "the drop is journaled, naming the note it dropped"
    );
    let queue = pending(&harness, agent);
    assert_eq!(queue.len(), 20, "the cap holds");
    assert_eq!(
        (
            queue.first().map(String::as_str),
            queue.last().map(String::as_str)
        ),
        (Some("note 1"), Some("note 20")),
        "the oldest went and the newest stayed: the latest instruction is the one that must \
         still reach the next boundary"
    );
}

// ---- the fsync window the cap is decided in ---------------------------------------------------

/// What the effect trace says about one hook.
///
/// Three states, not two, and the distinction is the parking itself: a reply that has not
/// been emitted is not the same as one emitted carrying nothing, and a test that collapsed them
/// could not tell a parked reply from an empty one.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// No reply for that hook is in the trace: it is parked behind a record that has not landed.
    Withheld,
    /// Answered, carrying this context.
    Given(Option<String>),
}

/// A harness whose persists are acknowledged by hand, so a test can hold an fsync open.
///
/// Positions are minted with `Lsn::from_parts(1, …)`: a real journal's first segment is 1, and a
/// fixture that taught segment 0 would teach a coordinate no reader ever sees (the rule `dudes-journal`'s own sample sweep enforces).
struct Hand {
    harness: Harness,
    agent: AgentId,
    next_offset: u64,
    acked: usize,
}

impl Hand {
    fn new() -> Self {
        let mut harness = Harness::new();
        let agent = harness.hire();
        // From here on nothing is acknowledged until a test says so.
        harness.fsync = false;
        Self {
            harness,
            agent,
            next_offset: 64,
            acked: 0,
        }
    }

    /// Persists emitted but not yet acknowledged, in emission order.
    fn outstanding(&self) -> VecDeque<PersistToken> {
        self.harness
            .trace
            .iter()
            .filter_map(|seen| match seen {
                Seen::Persist { token, .. } => Some(*token),
                _ => None,
            })
            .skip(self.acked)
            .collect()
    }

    fn command(&mut self, command: Command) {
        self.harness.step(Input::Command(command));
    }

    /// One hook event, handled but not followed up: nothing is acknowledged for it here.
    fn hook(&mut self, hook: HookId, kind: EventKind) {
        let agent = self.agent;
        self.harness.step(Input::Hook {
            hook,
            event: event(agent, kind),
        });
    }

    /// What the trace says about one hook so far.
    fn reply_for(&self, hook: HookId) -> Answer {
        self.harness
            .trace
            .iter()
            .find_map(|seen| match seen {
                Seen::Reply { hook: id, reply } if *id == hook => {
                    Some(Answer::Given(reply.context.clone()))
                }
                _ => None,
            })
            .unwrap_or(Answer::Withheld)
    }

    /// Acknowledge every outstanding persist, in emission order, at consecutive positions.
    fn ack_all(&mut self) {
        while let Some(token) = self.outstanding().pop_front() {
            let lsn = Lsn::from_parts(1, self.next_offset).expect("a segment-1 position");
            self.next_offset += 64;
            self.acked += 1;
            self.harness.step(self.harness.persisted(token, lsn));
        }
    }
}

/// Two steers at the cap inside one fsync window evict **two distinct** notes.
///
/// The window is real: `Supervisor::persist` parks the entry and the world does not move until
/// `on_persisted` folds it, so the depth the producer reads is the last *acknowledged* depth. A
/// producer that read only that would name the same front note twice — and the second
/// `SteerCleared`, being idempotent by design, would fold to nothing and leave the queue at 21.
///
/// Firing control: drop `steer_evicting` from `Supervisor::steer_evictions` and this test reports
/// two equal positions and a queue of 21.
#[test]
fn two_steers_at_the_cap_inside_one_fsync_window_evict_two_distinct_notes() {
    let mut hand = Hand::new();
    let agent = hand.agent;
    for n in 0..20 {
        hand.command(steer(agent, &format!("note {n}")));
    }
    hand.ack_all();
    assert_eq!(pending(&hand.harness, agent).len(), 20, "the queue is full");
    let [first, second, ..] = positions(&hand.harness, agent)[..] else {
        panic!("twenty notes are pending");
    };

    // Both commands are handled before either fsync is acknowledged.
    hand.command(steer(agent, "note 20"));
    hand.command(steer(agent, "note 21"));
    hand.ack_all();

    assert_eq!(
        cleared(&hand.harness),
        vec![first, second],
        "two drops, naming two different notes — not the same front note twice"
    );
    let queue = pending(&hand.harness, agent);
    assert_eq!(queue.len(), 20, "the cap holds across the window");
    assert_eq!(
        queue.last().map(String::as_str),
        Some("note 21"),
        "and both new notes landed"
    );
}

/// Two steers **one below** the cap inside one window still land at the cap.
///
/// The other half of the same projection: here nothing is evicted by the first command, so a
/// producer that counted only acknowledged notes sees 19 twice, evicts nothing, and folds to 21.
///
/// Firing control: drop `steer_appending` from `Supervisor::steer_evictions` and this test
/// reports a queue of 21 with no drop recorded.
#[test]
fn two_steers_one_below_the_cap_inside_one_window_still_land_at_the_cap() {
    let mut hand = Hand::new();
    let agent = hand.agent;
    for n in 0..19 {
        hand.command(steer(agent, &format!("note {n}")));
    }
    hand.ack_all();
    assert_eq!(pending(&hand.harness, agent).len(), 19, "one below the cap");
    let oldest = positions(&hand.harness, agent)[0];

    hand.command(steer(agent, "note 19"));
    hand.command(steer(agent, "note 20"));
    hand.ack_all();

    assert_eq!(
        cleared(&hand.harness),
        vec![oldest],
        "exactly one drop: the second note is what took the queue over"
    );
    assert_eq!(
        pending(&hand.harness, agent).len(),
        20,
        "the cap holds when the window starts below it"
    );
}

// ---- the consume is durable before the agent sees the note ------------------------------------

/// **No reply is emitted at all** until the consumption is on the disk.
///
/// The property is *parking*, not emission order, and the two are only distinguishable while an
/// acknowledgement is withheld — which is why this runs on [`Hand`]. Under `Harness::fsync = true`
/// the ack is synthesised the instant the persist is seen, so a machine that emitted the reply in
/// the same batch as the persist produces the same trace order as one that parks it. A test that
/// only asserted that order stays green against a mutant that removed the parking entirely.
///
/// Firing control: make the `Then::Reply` arm inert and push `Effect::Reply` beside the persist in
/// `on_hook`, and the first assertion below reports a reply that arrived before its record.
#[test]
fn no_reply_is_emitted_until_the_consume_is_on_the_disk() {
    let mut hand = Hand::new();
    let agent = hand.agent;
    hand.command(steer(agent, "hold the lock less"));
    hand.ack_all();

    hand.hook(HookId(9), EventKind::PromptSubmit);

    assert!(
        hand.harness.trace.iter().any(|seen| matches!(
            seen,
            Seen::Persist { entry, .. } if matches!(entry.record, Record::SteerConsumed { .. })
        )),
        "control: the consumption is on its way, so the withheld reply below is parked behind it \
         rather than never having been built"
    );
    assert_eq!(
        hand.reply_for(HookId(9)),
        Answer::Withheld,
        "the hook is not answered while the consumption is unacknowledged: an agent that read the \
         note before the record reached the disk could be told it again after a crash"
    );

    hand.ack_all();

    assert_eq!(
        hand.reply_for(HookId(9)),
        Answer::Given(Some("hold the lock less".to_owned())),
        "and the acknowledgement releases the parked reply, carrying the note"
    );
    assert!(
        pending(&hand.harness, agent).is_empty(),
        "which is also when the note leaves the queue"
    );
}

/// One note is never journaled as both dropped and delivered (`Record::SteerCleared`'s contract).
///
/// The interleaving is ordinary, not a crash: the queue is at the cap, a twenty-first note arrives
/// and names the front for eviction, and an admitted event lands before either fsync is
/// acknowledged. Without the guard in `pending_steer` the front note is still the front, so it is
/// delivered — and the journal then holds a `SteerCleared` saying it was dropped **undelivered**
/// beside a `SteerConsumed` saying it was delivered, for the same position. The queue lands right
/// either way; it is the record that would be lying.
///
/// Firing control: drop the `steer_evicting` guard from `pending_steer` and the two lists below
/// share the front note's position.
#[test]
fn a_note_is_never_both_dropped_and_delivered() {
    let mut hand = Hand::new();
    let agent = hand.agent;
    for n in 0..20 {
        hand.command(steer(agent, &format!("note {n}")));
    }
    hand.ack_all();
    let front = positions(&hand.harness, agent)[0];

    // The twenty-first names `front` for eviction; its `SteerCleared` has not folded yet.
    hand.command(steer(agent, "note 20"));
    hand.hook(HookId(21), EventKind::PromptSubmit);
    hand.ack_all();

    assert_eq!(
        cleared(&hand.harness),
        vec![front],
        "control: the drop happened, and named the front note"
    );
    let consumed = consumed(&hand.harness);
    assert!(
        consumed.iter().all(|lsn| *lsn != front),
        "no note is both dropped and delivered: cleared {:?} and consumed {consumed:?} name the \
         same position",
        vec![front],
    );

    // Withholding costs a beat, and no more: the note that displaced it lands next. Driven by
    // hand like everything else here — `reply_to` waits for an acknowledgement this harness does
    // not synthesise.
    hand.hook(HookId(22), EventKind::PromptSubmit);
    hand.ack_all();
    assert_eq!(
        hand.reply_for(HookId(22)),
        Answer::Given(Some("note 1".to_owned())),
        "the guard clears with the drop, and the new front is delivered"
    );
}

// ---- replay: a crash between delivery and consumption ------------------------------------------

/// Replay a journal into a fresh machine, then ask what the next admitted event carries.
fn replayed(journal: &[Record]) -> (Harness, AgentId) {
    let mut harness = Harness::new();
    for (position, record) in journal.iter().enumerate() {
        let lsn = Lsn::from_parts(1, 64 * (position as u64 + 1)).expect("a segment-1 position");
        harness.step(Input::Replayed {
            lsn,
            entry: Entry {
                at: Timestamp::from_unix_millis(1_787_394_600_000),
                record: record.clone(),
            },
        });
    }
    (harness, agent_id())
}

fn hired() -> Record {
    Record::AgentHired {
        character: None,
        accent: None,
        id: agent_id(),
        name: "Hector".to_owned(),
        role: "infra".to_owned(),
        department: "infra".to_owned(),
        provider: dudes_types::Provider::Claude,
        cwd: "/repo".into(),
        budget: dudes_proto::AgentBudget {
            usd_per_day: Some(dudes_types::Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        workspace: None,
        worktree: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

fn steer_record() -> Record {
    steer_record_saying("the note that was in flight")
}

fn steer_record_saying(text: &str) -> Record {
    Record::Steer {
        agent: agent_id(),
        text: text.to_owned(),
        provenance: Some(PeerProvenance::NoRecordedAgent),
    }
}

/// A `Steer` with **no** matching `SteerConsumed` is still pending after replay.
///
/// The crash-before-consume arm, and the half that keeps an operator's correction rather
/// than losing it: the daemon died between building the reply and the fsync landing, so nothing
/// says the agent ever saw it.
///
/// Firing control: its twin below, which appends the matching consumption to the same journal.
#[test]
fn a_crash_before_the_consume_leaves_the_note_pending() {
    let (mut harness, agent) = replayed(&[hired(), steer_record()]);

    assert_eq!(
        pending(&harness, agent),
        vec!["the note that was in flight".to_owned()],
        "the note survived the restart"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(10), EventKind::PromptSubmit),
        Some("the note that was in flight".to_owned()),
        "and is delivered at the next admitted event"
    );
}

/// The same journal **with** the consumption delivers nothing — the control for the arm above.
///
/// Delivery is at-most-once by decision: the daemon does not own the hook call it was answering,
/// so a restart cannot finish it, and re-delivering would be indistinguishable to the agent from
/// a fresh operator instruction — the defect arriving by another door.
#[test]
fn a_journal_holding_the_consume_delivers_nothing_after_replay() {
    let consumed_at = Lsn::from_parts(1, 128).expect("a segment-1 position");
    let (mut harness, agent) = replayed(&[
        hired(),
        steer_record(),
        Record::SteerConsumed {
            agent: agent_id(),
            steer_lsn: consumed_at,
        },
    ]);

    assert!(
        pending(&harness, agent).is_empty(),
        "the consumption removed the note it named"
    );
    assert_eq!(
        reply_to(&mut harness, agent, HookId(11), EventKind::PromptSubmit),
        None,
        "so replay delivers nothing: one delivery, whether or not the reply reached the vendor"
    );
}

/// A removal takes **the note it names**, a repeat takes nothing, and a stranger takes nothing.
///
/// This is the arm the whole `lsn` threading exists for, and it needs a journal whose removal is
/// **not** the front note. Otherwise "remove the note at `steer_lsn`" and "pop the front" agree on
/// every input a live run produces — the delivered note is always the oldest — and the position
/// the record carries is decoration that no test can tell from a counter.
///
/// The reason this test is shaped this way: a fold mutated to `pop_front()` passes
/// any test whose queue is empty by the time the repeat and the stranger folded. A no-op removal has to be folded **against a
/// queue that still holds notes** or it asserts nothing.
#[test]
fn a_removal_takes_the_note_it_names_and_a_repeat_or_stranger_takes_nothing() {
    let first = Lsn::from_parts(1, 128).expect("a segment-1 position");
    let second = Lsn::from_parts(1, 192).expect("a segment-1 position");
    let stranger = Lsn::from_parts(1, 4096).expect("a segment-1 position");
    let (harness, agent) = replayed(&[
        hired(),
        steer_record_saying("first: rebase"),
        steer_record_saying("second: squash"),
        // Names the *second* note, so a fold that popped the front would take the first.
        Record::SteerConsumed {
            agent: agent_id(),
            steer_lsn: second,
        },
        // Folded again, against a queue that still holds one note.
        Record::SteerConsumed {
            agent: agent_id(),
            steer_lsn: second,
        },
        // Names a position this queue never held, also against a non-empty queue.
        Record::SteerCleared {
            agent: agent_id(),
            steer_lsn: stranger,
        },
    ]);

    assert_eq!(
        pending(&harness, agent),
        vec!["first: rebase".to_owned()],
        "the removal took the note it named and left the other one alone"
    );
    assert_eq!(
        positions(&harness, agent),
        vec![first],
        "and the surviving note still answers to its own position"
    );
}

/// The mirror of the rule above: a note already **delivered** is never also journaled as dropped.
///
/// Same lie, opposite order. There the drop was in flight and the delivery followed; here the
/// consumption is in flight and the twenty-first note arrives behind it. The note is still in the
/// queue — the world does not move until the fold — so a cap that read the depth literally would
/// name that very position for eviction and the journal would hold a `SteerConsumed` and a
/// `SteerCleared` for it.
///
/// **Two assertions, because excluding the note is not enough on its own.** A fix that only kept
/// the consumed position out of the eviction candidates moves the eviction onto the next note
/// instead: a live, undelivered correction is dropped and the queue lands at 19, silently. So the
/// depth and the *absence of any* `SteerCleared` are asserted alongside the disjointness — the
/// right answer here is that nothing needed evicting at all, because the queue was already on its
/// way back under the cap.
///
/// Firing control: keep the `consuming` filter in `Supervisor::steer_evictions` and drop the
/// `spent` term from `projected`, and this reports one `SteerCleared` and a depth of 19.
#[test]
fn a_delivered_note_is_never_also_journaled_as_dropped() {
    let mut hand = Hand::new();
    let agent = hand.agent;
    for n in 0..20 {
        hand.command(steer(agent, &format!("note {n}")));
    }
    hand.ack_all();
    let front = positions(&hand.harness, agent)[0];

    // The consumption is persisted and not yet folded, so `note 0` is still in the queue.
    hand.hook(HookId(30), EventKind::PromptSubmit);
    assert_eq!(
        consumed(&hand.harness),
        vec![front],
        "control: the delivery happened and named the front note"
    );
    assert_eq!(
        pending(&hand.harness, agent).len(),
        20,
        "control: and the queue has not moved yet, which is what makes this interleaving possible"
    );

    // The twenty-first arrives inside that window.
    hand.command(steer(agent, "note 20"));
    hand.ack_all();

    let consumed = consumed(&hand.harness);
    let cleared = cleared(&hand.harness);
    assert!(
        cleared.iter().all(|lsn| !consumed.contains(lsn)),
        "no note is both delivered and dropped: consumed {consumed:?} and cleared {cleared:?}"
    );
    assert!(
        cleared.is_empty(),
        "and nothing needed dropping: the consumed note was already leaving, so the queue had \
         room for the new one — a drop here would be a live correction thrown away, {cleared:?}"
    );
    assert_eq!(
        pending(&hand.harness, agent).len(),
        20,
        "the cap holds, from the other side of the window"
    );
    assert_eq!(
        hand.reply_for(HookId(30)),
        Answer::Given(Some("note 0".to_owned())),
        "and the note that was consumed did reach the agent"
    );
}

/// A halted or paused agent is answered without spending a pending note.
#[test]
fn a_paused_agent_keeps_its_pending_note() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(steer(agent, "look at the deadlock")));
    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));

    assert_eq!(
        reply_to(&mut harness, agent, HookId(12), EventKind::PromptSubmit),
        None,
        "a paused agent is refused, and the refusal carries no note"
    );
    assert_eq!(
        pending(&harness, agent),
        vec!["look at the deadlock".to_owned()],
        "so the correction is still there when the loop resumes"
    );
}

/// A retired agent's recorded children stay owned. Without the retired arm a running agent would be allowed and handed the note, and
/// a paused one refused with `Deny`.
#[test]
fn a_retired_agent_is_halted_before_its_pause_and_spends_no_note() {
    for paused in [false, true] {
        let mut harness = Harness::new();
        let agent = harness.hire();
        harness.drive(Input::Command(steer(agent, "look at the deadlock")));
        if paused {
            harness.drive(Input::Command(Command::Pause {
                agent,
                browser: None,
            }));
        }
        harness.drive(Input::Command(Command::Retire {
            agent,
            reason: "the quarter ended".to_owned(),
            browser_retire: None,
        }));

        let kinds = [
            (HookId(31), EventKind::PromptSubmit),
            (
                HookId(32),
                EventKind::PreTool {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "make"}),
                },
            ),
        ];
        for (hook, kind) in kinds {
            assert_eq!(
                reply(&mut harness, agent, hook, kind),
                HookReply {
                    decision: HookDecision::Halt,
                    context: None,
                },
                "paused: {paused}; a retired agent's hook is halted and carries no note"
            );
        }
        assert!(consumed(&harness).is_empty(), "paused: {paused}");
        assert_eq!(
            pending(&harness, agent),
            vec!["look at the deadlock".to_owned()],
            "paused: {paused}; the note was not spent"
        );
    }
}
