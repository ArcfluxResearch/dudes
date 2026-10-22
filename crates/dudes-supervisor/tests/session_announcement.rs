//! only a vendor-confirmed session is resumable, and a turn issued or re-issued
//! while none exists announces a freshly minted id.
//!
//! Every assertion here is about *which* session a launch announces, and which one the turn ends
//! up recorded against — never about how many launches there are, which is invariant 2's and lives
//! in `persist_before_spawn.rs`.
//!
//! These tests are sans-io, and that costs nothing: the transcript-present branch (a
//! crash after `Spawn`, the child still writing its session file) and the transcript-absent
//! branch (a crash between `Persist` and `Spawn`) are *the same world state* — "no vendor-confirmed
//! session" — which is why the rule is one lookup. **Nothing here distinguishes those branches**;
//! that needs real children.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dudes_proto::goal::Evidence;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand, RecoveryRefusal};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{AdoptionKey, Command, Input, Liveness};
use dudes_types::{AgentId, GoalId, SessionId, TurnId};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Metadata, Subscriber};

type Captured = (String, Vec<(String, String)>);

#[derive(Default)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &SpanRecord<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap()
            .push((event.metadata().target().to_owned(), fields.0));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// The session flag one launch carries, read off the argv the adapter built.
///
/// Deliberately not `TurnSession`: the supervisor's choice is only observable to a vendor through
/// the argv, so the test reads what the child would have been handed.
#[derive(Debug, PartialEq, Eq)]
enum Announced {
    /// `--session-id <uuid>` — an id the daemon minted.
    Mint(String),
    /// `--resume <id>` — a session the vendor confirmed.
    Resume(String),
}

impl Announced {
    fn id(&self) -> &str {
        match self {
            Self::Mint(id) | Self::Resume(id) => id,
        }
    }
}

fn announced_in(spec: &LaunchSpec) -> Announced {
    let argv: Vec<String> = spec
        .argv
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    let after = |flag: &str| {
        argv.iter()
            .position(|part| part == flag)
            .and_then(|at| argv.get(at + 1))
            .cloned()
    };
    match (after("--session-id"), after("--resume")) {
        (Some(id), None) => Announced::Mint(id),
        (None, Some(id)) => Announced::Resume(id),
        other => panic!("a managed launch carries exactly one session flag: {other:?} in {argv:?}"),
    }
}

/// What the one launch for `turn` announced. Asserts the count on the way past, so a test that
/// reads an announcement can never be reading the wrong launch of two.
fn announcement(harness: &Harness, turn: TurnId) -> Announced {
    let spawns = harness.spawns_of(turn);
    assert_eq!(spawns.len(), 1, "one launch for {turn:?} in this trace");
    let Some(Seen::Spawn { spec, .. }) = spawns.first().copied() else {
        unreachable!("spawns_of yields only Seen::Spawn")
    };
    announced_in(spec)
}

/// Every session id announced so far, whichever flag carried it.
fn announced_so_far(harness: &Harness) -> Vec<String> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(announced_in(spec).id().to_owned()),
            _ => None,
        })
        .collect()
}

/// The id `TurnIssued` recorded for a turn — what it announced when it was *issued*.
fn issued_session(harness: &Harness, turn: TurnId) -> SessionId {
    harness
        .records()
        .find_map(|record| match record {
            Record::TurnIssued {
                turn: t, session, ..
            } if *t == turn => session.clone(),
            _ => None,
        })
        .expect("TurnIssued carries the session the turn announced")
}

fn key(pid: u32) -> AdoptionKey {
    AdoptionKey {
        pid,
        start_time: 900,
        boot: None,
    }
}

fn assert_spawn_then_confirmation(harness: &Harness, turn: TurnId, session: &SessionId) {
    let records: Vec<_> = harness.records().collect();
    assert!(matches!(
        records.as_slice(),
        [
            Record::TurnSpawned { turn: spawned, .. },
            Record::SessionLearned {
                turn: learned,
                session: confirmed,
                ..
            }
        ] if *spawned == turn && *learned == turn && confirmed == session
    ));
}

fn consent(
    harness: &mut Harness,
    agent: AgentId,
    source: TurnId,
    choice: RecoveryChoice,
    request: u8,
) {
    let launches = announced_so_far(harness);
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(
        announced_so_far(harness),
        launches,
        "ordinary Resume waits for consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .expect("the interrupted boundary offers recovery")
        .expected;
    assert_eq!(expected.work.turn, source);
    let request_id = Hex32::new([request; 32]);
    let handled = harness.recover(RecoveryCommand {
        request_id,
        digest: Hex32::new([request + 1; 32]),
        expected,
        choice,
        attribution: RecoveryAttribution::LocalOperator {
            pid: 7,
            uid: Some(42),
        },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.records().any(|record| matches!(record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == request_id)));
}

fn end_and_check(harness: &mut Harness, goal: GoalId, open: TurnId) {
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn: open,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: Some("not yet".into()),
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });
}

fn next_turn(harness: &mut Harness, agent: AgentId, goal: GoalId, open: TurnId) -> TurnId {
    end_and_check(harness, goal, open);
    let next = harness
        .open_turn(agent)
        .expect("the loop issued another turn");
    assert_ne!(next, open, "and it is a new turn, not the old one");
    next
}

fn fresh_after_unconfirmed(
    harness: &mut Harness,
    agent: AgentId,
    goal: GoalId,
    open: TurnId,
) -> TurnId {
    end_and_check(harness, goal, open);
    assert_eq!(
        harness.open_turn(agent),
        None,
        "the ended attempt waits for consent"
    );
    consent(harness, agent, open, RecoveryChoice::FreshWithRecap, 53);
    let next = harness
        .open_turn(agent)
        .expect("fresh consent admits the next turn");
    assert_ne!(next, open);
    next
}

#[test]
fn a_reissued_first_turn_mints_a_fresh_id() {
    let mut harness = Harness::new();
    // The crash: the record reached the disk, the process never started.
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    let recorded = issued_session(&harness, turn);
    assert_eq!(
        announcement(&harness, turn),
        Announced::Mint(recorded.as_str().to_owned()),
        "the live path minted an id and announced it"
    );
    let already = announced_so_far(&harness);

    harness.trace.clear();
    harness.launch = true;
    harness.restart();
    assert!(
        harness.spawns_of(turn).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut harness,
        agent,
        turn,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );

    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "one exact consent repeats the owed issue once"
    );
    let Announced::Mint(fresh) = announcement(&harness, turn) else {
        panic!("a session the vendor never named is never resumed")
    };
    assert_ne!(
        fresh,
        recorded.as_str(),
        "the re-issue mints afresh rather than re-announcing TurnIssued.session"
    );
    assert!(
        !already.contains(&fresh),
        "and it is an id nothing announced before: {already:?}"
    );

    let (session, reissued) = harness.spawned_session(turn);
    assert_eq!(
        session.as_ref().map(SessionId::as_str),
        Some(fresh.as_str()),
        "TurnSpawned records the id the child was launched on"
    );
    assert!(reissued, "and says the launch came from replay");
    assert_eq!(
        harness.supervisor.world().turns[&turn]
            .session
            .as_ref()
            .map(SessionId::as_str),
        Some(fresh.as_str()),
        "the turn row converges on the id it ran on"
    );
}

/// **The repeat axis.** One re-issue minting afresh is not the rule; the rule is that an
/// unconfirmed session is *never* resumed, however many turns go by. A second and a third turn
/// issued while no `SessionLearned` has arrived each announce a fresh mint — and then the vendor
/// names one back, and the next turn resumes exactly that id, because confirmation is the whole
/// difference.
///
/// The `SessionLearned` here carries **the id the turn announced**, which is not incidental: the
/// guard in `on_session_learned` misfires only when the confirmed id equals the announced one,
/// and that is every first turn. Driving this with any other id would pass over a broken guard.
#[test]
fn an_unconfirmed_session_is_never_resumed() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);

    let first = harness.open_turn(agent).expect("a turn was issued");
    let Announced::Mint(first_id) = announcement(&harness, first) else {
        panic!("the agent has no session at all yet")
    };

    let second = next_turn(&mut harness, agent, goal, first);
    let Announced::Mint(second_id) = announcement(&harness, second) else {
        panic!("nothing confirmed the first turn's session, so there is nothing to resume")
    };
    assert_ne!(
        second_id, first_id,
        "and it is not the unconfirmed id again"
    );

    let third = next_turn(&mut harness, agent, goal, second);
    let Announced::Mint(third_id) = announcement(&harness, third) else {
        panic!("still nothing confirmed: the rule does not tire after two turns")
    };
    assert_ne!(third_id, second_id);
    assert_ne!(third_id, first_id);

    // The vendor names the id back on `system/init`. That is what confirmation is.
    let confirmed = issued_session(&harness, third);
    assert_eq!(
        confirmed.as_str(),
        third_id,
        "the vendor confirms the id the turn announced — the case the guard gets wrong"
    );
    harness.trace.clear();
    harness.drive(Input::SessionLearned {
        turn: third,
        session: confirmed.clone(),
    });
    assert!(
        harness
            .records()
            .any(|record| matches!(record, Record::SessionLearned { .. })),
        "the confirmation of an id we minted is journaled, not swallowed"
    );
    assert_eq!(
        harness.supervisor.world().agents[&agent]
            .session
            .as_ref()
            .map(SessionId::as_str),
        Some(confirmed.as_str()),
        "and the agent row now holds a vendor-confirmed session"
    );

    let fourth = next_turn(&mut harness, agent, goal, third);
    assert_eq!(
        announcement(&harness, fourth),
        Announced::Resume(confirmed.as_str().to_owned()),
        "a confirmed session is resumed, and that is the only thing that is"
    );
}

/// **Session invalidation.** A re-issue beside a vendor-confirmed session announces a fresh `First`,
/// invalidates the old confirmation after spawn, and remains a re-issue: its `TurnSpawned` carries
/// `reissued: true`.
///
/// The four assertions keep the two halves together: the launch mint differs from A, the spawned
/// record carries that mint and `reissued: true`, and the acknowledged fold clears A. Dropping the
/// machine half resumes A; dropping the fold half leaves A available to every later turn.
#[test]
fn a_reissued_turn_on_a_confirmed_session_mints_and_invalidates() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");

    let confirmed = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });

    // The second turn resumes it — and then crashes between `Persist` and `Spawn`.
    harness.launch = false;
    let second = next_turn(&mut harness, agent, goal, first);
    assert_eq!(
        announcement(&harness, second),
        Announced::Resume(confirmed.as_str().to_owned()),
        "the confirmed session is what a later turn runs on"
    );

    harness.trace.clear();
    harness.launch = true;
    harness.restart();
    assert!(
        harness.spawns_of(second).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut harness,
        agent,
        second,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );

    let Announced::Mint(fresh) = announcement(&harness, second) else {
        panic!("a re-issue always starts a fresh session")
    };
    assert_ne!(fresh, confirmed.as_str());
    let (session, reissued) = harness.spawned_session(second);
    assert_eq!(
        session.as_ref().map(SessionId::as_str),
        Some(fresh.as_str()),
        "launched on the freshly minted id"
    );
    assert!(reissued, "and journaled as a re-issue");
    assert_eq!(
        harness.supervisor.world().agents[&agent].session,
        None,
        "the acknowledged re-issue invalidates the previous confirmation"
    );
}

#[test]
fn a_turn_after_an_unconfirmed_reissue_does_not_resume_the_old_session() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });

    harness.launch = false;
    let reissued = next_turn(&mut harness, agent, goal, first);
    harness.trace.clear();
    harness.launch = true;
    harness.restart();
    assert!(
        harness.spawns_of(reissued).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut harness,
        agent,
        reissued,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );

    let next = next_turn(&mut harness, agent, goal, reissued);
    let Announced::Mint(_) = announcement(&harness, next) else {
        panic!("the turn after an unconfirmed re-issue starts fresh")
    };
}

#[test]
fn an_invalidated_session_announces_first() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });

    let resumed = next_turn(&mut harness, agent, goal, first);
    assert_eq!(
        announcement(&harness, resumed),
        Announced::Resume(confirmed.as_str().to_owned())
    );
    let next = fresh_after_unconfirmed(&mut harness, agent, goal, resumed);
    assert!(matches!(announcement(&harness, next), Announced::Mint(_)));
}

#[test]
fn a_resumed_turn_that_the_vendor_confirmed_keeps_its_session() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });

    let resumed = next_turn(&mut harness, agent, goal, first);
    harness.drive(Input::SessionLearned {
        turn: resumed,
        session: confirmed.clone(),
    });
    let next = next_turn(&mut harness, agent, goal, resumed);
    assert_eq!(
        announcement(&harness, next),
        Announced::Resume(confirmed.as_str().to_owned())
    );
}

#[test]
fn a_vanished_or_killed_turn_does_not_invalidate() {
    for killed in [false, true] {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
        let first = harness.open_turn(agent).expect("a turn was issued");
        let confirmed = issued_session(&harness, first);
        harness.drive(Input::SessionLearned {
            turn: first,
            session: confirmed.clone(),
        });
        let resumed = next_turn(&mut harness, agent, goal, first);

        if killed {
            harness.drive(Input::Command(Command::Halt {
                agent,
                text: Some("stop".into()),
            }));
            harness.drive(Input::ChildExited {
                final_reply: None,
                elapsed_ms: None,
                turn: resumed,
                exit: ExitStatus::UNKNOWN,
                usage: None,
                model_usage: BTreeMap::new(),
                outcome: TurnOutcome::Completed,
                last_message: None,
            });
        } else {
            harness.drive(Input::Liveness {
                turn: resumed,
                liveness: Liveness::Gone,
            });
        }

        assert_eq!(
            harness.supervisor.world().agents[&agent].session.as_ref(),
            Some(&confirmed),
            "neither non-demoting outcome clears the confirmation"
        );
    }
}

fn assert_first_turn_accepts_assigned_session() {
    let mut first_turn = Harness::new();
    let (first_agent, _) = first_turn.hire_with_goal(&["npm", "test"]);
    let first = first_turn
        .open_turn(first_agent)
        .expect("a turn was issued");
    let assigned = SessionId::new("vendor-assigned-first-id");
    first_turn.drive(Input::SessionLearned {
        turn: first,
        session: assigned.clone(),
    });
    assert_eq!(
        first_turn.supervisor.world().agents[&first_agent]
            .session
            .as_ref(),
        Some(&assigned),
        "a first turn may bind the id the vendor assigned"
    );
}

#[test]
fn a_resume_confirmed_as_another_id_invalidates_and_does_not_rebind() {
    let mut mismatch = Harness::new();
    let (agent, goal) = mismatch.hire_with_goal(&["npm", "test"]);
    let first = mismatch.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&mismatch, first);
    mismatch.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });
    let resumed = next_turn(&mut mismatch, agent, goal, first);
    let other = SessionId::new("vendor-returned-another-id");
    mismatch.trace.clear();
    mismatch.drive(Input::SessionLearned {
        turn: resumed,
        session: other.clone(),
    });
    assert!(!mismatch.records().any(|record| matches!(
        record,
        Record::SessionLearned { session, .. } if session == &other
    )));
    let next = fresh_after_unconfirmed(&mut mismatch, agent, goal, resumed);
    assert!(matches!(announcement(&mismatch, next), Announced::Mint(_)));

    assert_first_turn_accepts_assigned_session();

    let mut before_spawn = Harness::new();
    let (reissue_agent, reissue_goal) = before_spawn.hire_with_goal(&["npm", "test"]);
    let first = before_spawn
        .open_turn(reissue_agent)
        .expect("a turn was issued");
    let old = issued_session(&before_spawn, first);
    before_spawn.drive(Input::SessionLearned {
        turn: first,
        session: old.clone(),
    });
    before_spawn.launch = false;
    let reissued = next_turn(&mut before_spawn, reissue_agent, reissue_goal, first);
    before_spawn.trace.clear();
    before_spawn.restart();
    assert!(
        before_spawn.spawns_of(reissued).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut before_spawn,
        reissue_agent,
        reissued,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );
    let Announced::Mint(fresh) = announcement(&before_spawn, reissued) else {
        panic!("the re-issue announces first")
    };
    assert!(!before_spawn.records().any(|record| matches!(
        record,
        Record::TurnSpawned { turn, .. } if *turn == reissued
    )));
    let fresh = SessionId::new(fresh);
    before_spawn.trace.clear();
    before_spawn.drive(Input::SessionLearned {
        turn: reissued,
        session: fresh.clone(),
    });
    assert_eq!(
        before_spawn.supervisor.world().agents[&reissue_agent]
            .session
            .as_ref(),
        Some(&old),
        "the current confirmation waits behind the durable re-issue carrier"
    );
    assert_eq!(
        before_spawn.records().count(),
        0,
        "a pre-spawn confirmation is buffered rather than journaled out of order"
    );

    before_spawn.drive(Input::Spawned {
        turn: reissued,
        key: key(4242),
        mcp_grant: None,
    });
    let (spawned_session, was_reissued) = before_spawn.spawned_session(reissued);
    assert_eq!(spawned_session.as_ref(), Some(&fresh));
    assert!(was_reissued);
    assert_eq!(
        before_spawn.supervisor.world().agents[&reissue_agent]
            .session
            .as_ref(),
        Some(&fresh),
        "the confirmation binds after TurnSpawned durably clears the prior attempt"
    );
    assert_spawn_then_confirmation(&before_spawn, reissued, &fresh);

    let next = next_turn(&mut before_spawn, reissue_agent, reissue_goal, reissued);
    assert_eq!(
        announcement(&before_spawn, next),
        Announced::Resume(fresh.as_str().to_owned()),
        "the accepted fresh confirmation remains the next turn's resume licence"
    );
}

#[test]
fn a_resume_mismatch_warning_carries_no_vendor_session_bytes() {
    const ANNOUNCED: &str = "announced\n\u{1b}[31mSENSITIVE_SESSION_HANDLE_A\u{1b}[0m";
    const REPORTED: &str = "reported\r\n\u{1b}[2JSENSITIVE_SESSION_HANDLE_B";

    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    harness.drive(Input::SessionLearned {
        turn: first,
        session: SessionId::new(ANNOUNCED),
    });
    let resumed = next_turn(&mut harness, agent, goal, first);
    assert_eq!(
        announcement(&harness, resumed),
        Announced::Resume(ANNOUNCED.to_owned()),
        "the hostile handle reaches the mismatch path as the resumed session"
    );

    let capture = Capture::default();
    let events = Arc::clone(&capture.0);
    tracing::subscriber::with_default(capture, || {
        harness.drive(Input::SessionLearned {
            turn: resumed,
            session: SessionId::new(REPORTED),
        });
    });
    let events = events.lock().unwrap();
    let (_, fields) = events
        .iter()
        .find(|(_, fields)| {
            fields.iter().any(|(name, value)| {
                name == "message"
                    && value.contains("the vendor did not confirm the session this turn resumed")
            })
        })
        .expect("the fixed resume-mismatch warning");
    let rendered = fields
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !rendered.contains(ANNOUNCED),
        "announced handle leaked: {rendered}"
    );
    assert!(
        !rendered.contains(REPORTED),
        "reported handle leaked: {rendered}"
    );
    assert!(
        !rendered.contains('\n'),
        "newline reached the warning: {rendered:?}"
    );
    assert!(
        !rendered.contains('\u{1b}'),
        "ANSI reached the warning: {rendered:?}"
    );

    let mut names: Vec<_> = fields.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["agent", "category", "message", "turn"]);
    assert!(
        fields.iter().any(|(name, value)| {
            name == "category" && value.contains("resume_session_mismatch")
        })
    );
}

/// **The vendor names an id the turn never announced.** On the providers that assign their own
/// session, the id does not exist until the process has started, so what comes back is *not* the
/// one `--session-id` carried. Reconciling the turn onto it is the whole reason `SessionLearned`
/// touches `TurnRow::session` at all — the turn row is documented to converge on the id the child
/// ran on, and this is the only trace where converging means *changing*.
///
/// Both `SessionLearned` drives above confirm **the announced id**, for the reason
/// `an_unconfirmed_session_is_never_resumed` sets out: driving them with any other id would pass
/// over a broken guard. That is right for what they assert, and it leaves the reconciliation
/// value-identical in both — the row already holds the confirmed id before the fold runs, so
/// nothing there can see the write.
///
/// Hence the before/after pair below. Only a `SessionLearned` fold that writes `TurnRow::session`
/// satisfies both halves: the row holds the announced id up to the confirmation, and the vendor's
/// id after it.
#[test]
fn a_confirmed_id_the_turn_never_announced_lands_on_the_turn_row() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    let announced = issued_session(&harness, turn);
    assert_eq!(
        announcement(&harness, turn),
        Announced::Mint(announced.as_str().to_owned()),
        "the daemon minted an id and handed it to the child"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].session.as_ref(),
        Some(&announced),
        "and until the vendor speaks, that is what the turn row holds"
    );

    // The vendor assigned its own instead, and names it back on `system/init`.
    let confirmed = SessionId::from("f4a9c0de-vendor-assigned-not-ours");
    let already = announced_so_far(&harness);
    assert!(
        !already.iter().any(|id| id == confirmed.as_str()),
        "the premise of this test: no launch announced this id — {already:?}"
    );
    harness.drive(Input::SessionLearned {
        turn,
        session: confirmed.clone(),
    });

    assert_eq!(
        harness.supervisor.world().turns[&turn].session.as_ref(),
        Some(&confirmed),
        "the turn row converges on the id the child ran on, not the one it was asked to run on"
    );
}

#[test]
fn a_confirmation_from_an_earlier_attempt_cannot_license_a_later_reissue() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });

    harness.launch = false;
    let stranded = next_turn(&mut harness, agent, goal, first);
    assert!(!harness.records().any(|record| matches!(
        record,
        Record::TurnSpawned { turn, .. } if *turn == stranded
    )));
    let issued_prefix = harness.journal.clone();
    harness.drive(Input::SessionLearned {
        turn: stranded,
        session: confirmed.clone(),
    });
    assert!(harness.supervisor.world().turns[&stranded].session_confirmed);

    harness.trace.clear();
    harness.launch = true;
    harness.restart();
    assert_eq!(
        harness.supervisor.recovery_opportunity(agent),
        Err(RecoveryRefusal::NotRecoverable),
        "a confirmation without a durable spawn cannot license a repeat"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(harness.spawns_of(stranded).is_empty());

    let mut harness = Harness::from_prefix(issued_prefix);
    harness.drive(Input::Started);
    assert!(
        harness.spawns_of(stranded).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut harness,
        agent,
        stranded,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );

    let Announced::Mint(fresh) = announcement(&harness, stranded) else {
        panic!("replay re-issues on a fresh session")
    };
    let fresh = SessionId::new(fresh);
    assert_ne!(fresh, confirmed);
    let (spawned, reissued) = harness.spawned_session(stranded);
    assert_eq!(spawned.as_ref(), Some(&fresh));
    assert!(reissued);
    assert_eq!(
        harness.supervisor.world().agents[&agent].session,
        None,
        "an earlier attempt's confirmation must not preserve the superseded session"
    );
    assert!(
        !harness.supervisor.world().turns[&stranded].session_confirmed,
        "the later attempt still needs its own confirmation"
    );
}

#[test]
fn an_early_assigning_confirmation_is_buffered_once_and_resumed() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let old = issued_session(&harness, first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: old.clone(),
    });

    harness.launch = false;
    let reissued = next_turn(&mut harness, agent, goal, first);
    harness.trace.clear();
    harness.restart();
    assert!(
        harness.spawns_of(reissued).is_empty(),
        "restart waits for consent"
    );
    consent(
        &mut harness,
        agent,
        reissued,
        RecoveryChoice::RepeatIssuedFresh,
        51,
    );
    let Announced::Mint(announced) = announcement(&harness, reissued) else {
        panic!("the re-issue announces first")
    };
    let assigned = SessionId::new("vendor-assigned-current-reissue");
    let ignored = SessionId::new("later-duplicate-init");
    assert_ne!(assigned.as_str(), announced);

    harness.trace.clear();
    harness.drive(Input::SessionLearned {
        turn: reissued,
        session: assigned.clone(),
    });
    harness.drive(Input::SessionLearned {
        turn: reissued,
        session: ignored,
    });
    assert_eq!(harness.records().count(), 0);

    harness.drive(Input::Spawned {
        turn: reissued,
        key: key(4343),
        mcp_grant: None,
    });
    let learned: Vec<_> = harness
        .records()
        .filter_map(|record| match record {
            Record::SessionLearned { session, .. } => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(learned, [&assigned]);
    assert_eq!(
        harness.supervisor.world().agents[&agent].session.as_ref(),
        Some(&assigned)
    );
    assert!(harness.supervisor.world().turns[&reissued].session_confirmed);

    let next = next_turn(&mut harness, agent, goal, reissued);
    assert_eq!(
        announcement(&harness, next),
        Announced::Resume(assigned.as_str().to_owned())
    );
}
