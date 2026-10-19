//! **The pin on invariant 1** — *persistent state exists only as records appended to the
//! journal; everything else is a projection `dudes replay` can rebuild.* Checkpointing changes
//! no projection, so a journal the writer stamped and one it did not rebuild the same rows.
//!
//! The writer emits a `Record::Checkpoint` every `JournalOptions::checkpoint_every` appends and
//! **nothing reads one**: `replay()` applies it as a no-op, `agents`, `goals` and `turns` name it
//! in an arm that does nothing, and `budget` never reaches a record arm for it. A producer with
//! no consumer must not read as a live mechanism, and the way to keep it honest is a check that
//! goes red the day a consumer lands.
//!
//! **Its disposition, so nobody has to guess.** The proposition here — checkpointing changes no
//! projection — is false by design the moment a fold reads a checkpoint, so it has no true form
//! to be updated into. **The change that lands a consumer deletes this file** and puts that
//! consumer's own properties in its place. It does not repair it, and it does not weaken an
//! assertion until it passes. A pin whose subject is scheduled to disappear has to say so.
//!
//! One record sequence, written twice — once with no checkpoints, once with one every three
//! appends — and replayed into two fresh stores.
//!
//! **The discriminator is `ReplayReport.applied`, asserted first.** Row equality alone also
//! holds for a writer that emits nothing, because then the two logs are the same log; `applied`
//! is what proves the second one really carries extra records for the projection to be
//! indifferent to.
//!
//! **A stored position is checked for content identity, never for coordinate equality, and
//! nothing is normalised.** A position moves when frames are written in front of it, so
//! requiring two runs to agree on one is requiring the writer to have written nothing — and
//! copying one run's position into the other's row before comparing would remove that field from
//! the assertion, with nothing else asserting it. What must hold is that each stored position
//! still **resolves to the same record in its own log**: `TurnRow.lsn_issued` to that turn's
//! `TurnIssued`, `GoalRow.last_result.lsn` to that goal's `PredicateResult`. The rule is:
//! `lsn` locates a record; the frame's `crc32c` says whether the record there differs from the
//! one applied. Neither alone identifies it. Resolution reads the frame back, which is what a
//! checksum could not do and a re-read can.
//!
//! Those two fields are **examples, not the list**. [`resolve_positions`] finds a position by the
//! shape of its key, so a row that gains one is covered the day it is added.

use std::collections::BTreeMap;
use std::path::Path;

use dudes_journal::{
    Fsync, JournalOptions, Projections, Reader, ReplayReport, SpendViews, frame, open, replay,
};
use dudes_proto::record::{Record, RecordKind, samples};
use dudes_proto::{Entry, Goal};
use dudes_types::{AgentId, GoalId, Lsn, Timestamp, UtcDay};
use serde_json::Value;

/// A stamp far enough from the epoch to have the width a real one has.
const AT_BASE: i64 = 1_787_394_600_000;

/// A checkpoint every three appends: small enough that the story crosses it repeatedly, and not
/// a divisor of the story's length, so the last checkpoint is not the last frame.
const EVERY: u64 = 3;

// ---- the corpus ---------------------------------------------------------------------------

/// Where a record kind sits in [`story`], or why it is not in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// Written by the story, at this point in the order.
    At(u8),
    /// Not written, with the reason. An arm rather than an omission, so the reason is visible
    /// and a kind cannot join the silent set by being forgotten.
    ///
    /// Exactly one kind is excused today — `Checkpoint` itself, for the reason on its arm — and
    /// `the_story_covers_the_whole_corpus` pins that set so a second one cannot join it quietly.
    Out(&'static str),
}

const RECOVERY_PREFIX_PLACEMENT: Placement = Placement::Out(
    "joined recovery sources are covered by recovery_facts::matrix::invalidation_story_survives_automatic_checkpoints_rotation_and_physical_reopen at actual required/source positions",
);

/// Every kind, placed. **The match has no wildcard**, so a kind added to the contract does not
/// compile until someone places it or states why it is out.
///
/// This is the half of the pin that fails *silent* otherwise. A field written only by a record
/// kind the story never writes is invisible to every assertion in this file — measured, a
/// `question_lsn` on `AgentRow` written in the `HumanQuestion` arm left the suite green while a
/// field the story does reach reddened it. A hand-listed corpus is a standing gap; this is not.
fn placement(kind: RecordKind) -> Placement {
    use Placement::At;
    match kind {
        // The agent, its token, and the goal it is given.
        RecordKind::AgentHired => At(0),
        RecordKind::HookTokenMinted => At(2),
        RecordKind::GoalSet => At(3),
        // One managed turn: decided, issued, spawned, its session learned, steered.
        RecordKind::LoopDecided => At(4),
        RecordKind::TurnIssued => At(5),
        RecordKind::TurnSpawned => At(6),
        RecordKind::SessionLearned => At(7),
        RecordKind::Steer => At(8),
        // A question and its answer, then an attach and its detach.
        RecordKind::HumanQuestion => At(9),
        RecordKind::HumanAnswer => At(10),
        RecordKind::Attached => At(11),
        RecordKind::Detached => At(12),
        // Memory, the board, the mailbox and the closing-time protocol.
        RecordKind::MemoryAppend => At(13),
        RecordKind::MemoryCondense => At(14),
        RecordKind::CardCreated => At(15),
        RecordKind::CardTransition => At(16),
        RecordKind::Claim => At(17),
        RecordKind::Mail => At(18),
        RecordKind::MailState => At(19),
        RecordKind::ClosingTime => At(20),
        RecordKind::LiveActivityObserved => At(21),
        // The one kind the story must **not** write, and the reason is measured rather than
        // aesthetic. A hand-written `Checkpoint` appears in *both* logs, so a fold that stopped
        // being inert would fire in both — and any write that is idempotent (setting a flag,
        // stamping a constant) then lands identically on each side and compares equal. With a
        // `Checkpoint` at this position, an arm setting `paused = true` leaves the suite green;
        // with the kind excused it goes red. The whole subject of
        // this file is the checkpoints the *writer* adds, and they are only visible as a
        // difference while one log has them and the other does not.
        RecordKind::Checkpoint => Placement::Out(
            "a hand-written copy fires a non-inert fold in both logs, so an idempotent write \
             compares equal and the pin misses it; the writer's own are the subject here",
        ),
        RecordKind::KillDeferred => At(22),
        // The end: observed, priced, judged, and the breaker beaten on the result.
        RecordKind::TurnEnded => At(23),
        RecordKind::BudgetDecision => At(24),
        RecordKind::PredicateResult => At(25),
        RecordKind::BreakerBeat => At(26),
        // Control flags, both directions.
        RecordKind::AgentHeld => At(27),
        RecordKind::Pause => At(28),
        RecordKind::Halt => At(29),
        RecordKind::Resume => At(30),
        // The goal moves to the second agent, is cleared, and the first agent retires.
        RecordKind::GoalReassigned => At(31),
        RecordKind::AgentLookChanged => At(32),
        RecordKind::GoalCleared => At(33),
        RecordKind::AgentRetired => At(34),
        // Audit-only conditioned dispositions are inert in every projection.
        RecordKind::ConditionedEffectSuppressed => At(35),
        RecordKind::MemoryAppendQuarantined => At(36),
        // The PTY byte path's audit pair, inert in every projection for the reason `world.rs`
        // gives: a write is the continuation of the call that made it and replay re-issues none.
        RecordKind::PtyWriteRequested => At(37),
        RecordKind::PtyWriteCompleted => At(38),
        // A confirmed PTY close is likewise an audit pair; `Detached` owns the lifecycle fold.
        RecordKind::PtyCloseRequested => At(41),
        RecordKind::PtyCloseCompleted => At(42),
        // A note spent and a note dropped. Inert in every projection for the same reason as the
        // pair above: the steer queue lives in the supervisor's in-memory fold and has no redb
        // column, so neither kind writes anything a checkpoint could differ on. Placed anyway,
        // because a kind the story never writes is invisible to every assertion in this file.
        RecordKind::SteerConsumed => At(39),
        RecordKind::SteerCleared => At(40),
        // An attached child's key. Only the supervisor's `World` folds it, and no redb projection
        // has a column for it, so its place in the story changes nothing a checkpoint could hold.
        RecordKind::PtySpawned => At(43),
        RecordKind::AgentRenamed => At(44),
        RecordKind::ConversationDispatchUncertain
        | RecordKind::ConversationContextResetQueued
        | RecordKind::ConversationQuestion
        | RecordKind::ConversationAnswerQueued
        | RecordKind::ConversationMessageQueued
        | RecordKind::ConversationTurnDecided
        | RecordKind::ConversationTurnIssued
        | RecordKind::BrowserNoteQueued
        | RecordKind::BrowserNoteCancelled
        | RecordKind::BrowserNoteIssuedCancelled
        | RecordKind::BrowserNoteDispatchIntent
        | RecordKind::BrowserNoteReissueCancelled
        | RecordKind::BrowserNoteContextEmitted
        | RecordKind::NoteTurnDecided
        | RecordKind::TaughtNoteTurnDecided
        | RecordKind::TaughtNoteOnlyTurnIssued
        | RecordKind::NoteOnlyTurnIssued
        | RecordKind::TurnIssuedWithBrowserNote => Placement::Out(
            "matched browser lifecycles have independent prefix and checkpoint equality tests",
        ),
        // The daemon's own pre-spawn admission audit for a memory-condense model call
        //: inert in every projection for the same reason `BudgetDecision` is.
        RecordKind::MemoryCondenseModelAdmitted => At(45),
        // A Floor audit record: inert in every projection, like the admission audit above.
        RecordKind::FloorPeerRefused => At(46),
        RecordKind::WorktreeLeased | RecordKind::WorktreeRemoved => Placement::Out(
            "the lease lifecycle needs its own agent and removal order; tests/worktrees.rs replays it with and without checkpoints",
        ),
        RecordKind::MailWakeTurnDecided | RecordKind::MailWakeOnlyTurnIssued => Placement::Out(
            "matched wake authority uses the checkpointed same-journal rebuild case below; standalone samples do not form valid joins",
        ),
        RecordKind::TicketCreated
        | RecordKind::TicketEdited
        | RecordKind::TicketAssigned
        | RecordKind::TicketStarted
        | RecordKind::TicketRemoved
        | RecordKind::TicketBaselineRecorded
        | RecordKind::TicketReviewStamped => Placement::Out(
            "a ticket's records need the card they name; tests/tickets.rs replays the lifecycle with and without checkpoints",
        ),
        RecordKind::InstructionObservationStarted
        | RecordKind::InstructionObservationFinished
        | RecordKind::TicketInstructionReviewed
        | RecordKind::InstructionAttemptBoundarySettled => Placement::Out(
            "instruction authority needs exact joined issue, spawn, end and review positions; standalone samples do not supply those joins",
        ),
        RecordKind::RecoveryRequired
        | RecordKind::RecoveryAuthorized
        | RecordKind::RecoveryInvalidated => RECOVERY_PREFIX_PLACEMENT,
        RecordKind::AgentMcpServersChanged | RecordKind::McpLaunchGranted => Placement::Out(
            "MCP authority requires its edited policy and physical joins; required frame and cache rebuild tests cover those records separately",
        ),
    }
}

/// Where the second hire goes. Not a kind of its own — an extra `AgentHired` — and it earns its
/// place twice.
///
/// It gives `GoalReassigned`'s `to` an agent this journal actually hired. And it is **the agent
/// row that survives the ending**: the story closes with `AgentRetired`, which *removes* a row
/// (`agents.rs`), so a fold that wrote to the first agent on one of the last checkpoints would
/// write to a row that is no longer there. Measured — that exact mutation is green against the
/// retired agent and red against this one. Without a surviving row the corpus's own ending would
/// blunt the pin.
const SECOND_HIRE_AT: u8 = 1;

/// The `AgentHired` sample with the id `GoalReassigned` reassigns to. Read out of that sample
/// rather than written down, because the corpus's id helpers are crate-private to `dudes-proto`.
fn second_hire() -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired { id, .. } = &mut hired else {
        panic!("the AgentHired sample is not an AgentHired")
    };
    *id = other_agent();
    hired
}

/// One working life, in an order the single writer could have produced, covering **every**
/// record kind in the contract. Each entry gets a distinct `at` inside one UTC day.
fn story() -> Vec<Entry> {
    let mut placed: Vec<(u8, Record)> = RecordKind::ALL
        .into_iter()
        .filter_map(|kind| match placement(kind) {
            Placement::At(n) => Some((n, samples::record(kind))),
            Placement::Out(_) => None,
        })
        .collect();
    placed.push((SECOND_HIRE_AT, second_hire()));
    placed.sort_by_key(|(n, _)| *n);
    placed
        .into_iter()
        .enumerate()
        .map(|(i, (_, record))| Entry {
            at: Timestamp::from_unix_millis(AT_BASE + i64::try_from(i).unwrap()),
            record,
        })
        .collect()
}

fn append_completed_mail_receipt(
    writer: &mut dudes_journal::Writer,
    at: Timestamp,
) -> (Lsn, dudes_proto::MailRef) {
    use dudes_proto::{LoopDecision, MailFrom, MailRef, MailTo};
    use dudes_types::MailState;
    let mut append = |record, at| writer.append(&Entry { at, record }).unwrap();
    append(samples::record(RecordKind::AgentHired), at);
    append(second_hire(), at);
    append(samples::record(RecordKind::GoalSet), at);
    append(samples::record(RecordKind::PredicateResult), at);
    let completion_lsn = append(
        Record::LoopDecided {
            agent: agent(),
            input_digest: "ordinary-done".into(),
            gate_trace: Vec::new(),
            decision: LoopDecision::Done,
        },
        at,
    );
    let mut mail = samples::record(RecordKind::Mail);
    let Record::Mail {
        id,
        from,
        to,
        state,
        ..
    } = &mut mail
    else {
        unreachable!();
    };
    let id = *id;
    *from = MailFrom::Agent {
        agent: other_agent(),
    };
    *to = MailTo::Agent { agent: agent() };
    *state = MailState::Durable;
    let receipt = MailRef {
        id,
        lsn: append(mail, at),
    };
    (completion_lsn, receipt)
}

fn checkpoint_wake_trace() -> Vec<dudes_proto::GateStep> {
    use dudes_proto::{Gate, GateStep};
    Gate::ORDER
        .into_iter()
        .map(|gate| GateStep {
            gate,
            fired: gate == Gate::Work,
            note: None,
        })
        .collect()
}

fn write_checkpointed_wake_prefix(journal: &Path) {
    use dudes_proto::record::Hex32;
    use dudes_proto::{
        MailWakeAdmission, MailWakeDecision, MailWakeDigest, MailWakeDigestDomain, MailWakeIssue,
        MailWakePlan, MailWakePurpose, MailWakeTarget, MailWakeTask,
    };
    use dudes_types::{MailState, Mode, ModelSelection};

    let mut writer = open(
        journal,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 3,
            ..JournalOptions::default()
        },
    )
    .unwrap()
    .0;
    let at = Timestamp::from_unix_millis(AT_BASE);
    let (completion_lsn, receipt) = append_completed_mail_receipt(&mut writer, at);
    let id = receipt.id;
    let mut append = |record, at| writer.append(&Entry { at, record }).unwrap();
    let task = MailWakeTask::new(1).unwrap();
    let root_lsn = append(
        Record::MailWakeTurnDecided(MailWakeDecision {
            agent: agent(),
            goal: goal(),
            completion_lsn,
            purpose: MailWakePurpose::FreshPreparation {
                plan: MailWakePlan {
                    trigger: receipt,
                    frontier: receipt.lsn,
                    unread_count: 1,
                    task: task.clone(),
                    targets: vec![MailWakeTarget {
                        mail: receipt,
                        original_count: 0,
                        target_count: 1,
                    }],
                },
            },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Fresh,
                value: Hex32::new([1; 32]),
            },
            gate_trace: checkpoint_wake_trace(),
            decision: MailWakeAdmission::Issue { task: task.clone() },
        }),
        at,
    );
    append(
        Record::MailState {
            id,
            state: MailState::Notified { n: 1 },
        },
        at,
    );
    let late = Timestamp::from_unix_millis(AT_BASE + 1);
    let lsn_of_decision = append(
        Record::MailWakeTurnDecided(MailWakeDecision {
            agent: agent(),
            goal: goal(),
            completion_lsn,
            purpose: MailWakePurpose::ExistingPreparation { root_lsn },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Resume,
                value: Hex32::new([2; 32]),
            },
            gate_trace: checkpoint_wake_trace(),
            decision: MailWakeAdmission::Issue { task: task.clone() },
        }),
        late,
    );
    let Record::TurnIssued { turn, .. } = samples::record(RecordKind::TurnIssued) else {
        unreachable!();
    };
    append(
        Record::MailWakeOnlyTurnIssued(MailWakeIssue {
            agent: agent(),
            turn,
            goal: goal(),
            completion_lsn,
            root_lsn,
            lsn_of_decision,
            task,
            session: None,
            mode: Mode::Managed,
            model_selection: ModelSelection::default(),
            answers: Vec::new(),
        }),
        late,
    );
    drop(writer);
}

#[test]
fn checkpointed_wake_prefix_reopens_and_rebuilds_exactly() {
    use dudes_types::GoalStatus;
    let owned = tempfile::tempdir().unwrap();
    let journal = owned.path().join("journal");
    write_checkpointed_wake_prefix(&journal);
    let reader = Reader::open(&journal).unwrap().0;
    let records: Vec<_> = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(records.len(), 13);
    assert_eq!(
        records
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::Checkpoint { .. }))
            .count(),
        3
    );
    let cache = owned.path().join("state.redb");
    let mut projected = Projections::open(&cache).unwrap();
    assert_eq!(replay(&reader, &mut projected).unwrap().applied, 13);
    let activity = projected.activity().unwrap();
    assert_eq!(
        serde_json::to_value(activity.last().unwrap()).unwrap(),
        serde_json::json!({
            "lsn": records.last().unwrap().0.0.to_string(),
            "at": AT_BASE + 1,
            "kind": "turn_issued",
            "agent": {"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "name": "Kim"},
        }),
    );
    assert_eq!(
        projected.goal(goal()).unwrap().unwrap().status,
        GoalStatus::Achieved
    );
    assert_eq!(projected.goal(goal()).unwrap().unwrap().turns_on_goal, 1);
    assert_eq!(projected.agent(agent()).unwrap().unwrap().turns, 1);
    let expected = projected.export().unwrap();
    assert!(
        expected
            .lines()
            .any(|line| line.starts_with("mail_wake_by_day ") && line.ends_with(" 1"))
    );
    drop(projected);
    let mut reopened = Projections::open(&cache).unwrap();
    assert_eq!(replay(&reader, &mut reopened).unwrap().applied, 0);
    assert_eq!(reopened.export().unwrap(), expected);
    let mut rebuilt = Projections::in_memory().unwrap();
    assert_eq!(replay(&reader, &mut rebuilt).unwrap().applied, 13);
    assert_eq!(rebuilt.activity().unwrap(), activity);
    assert_eq!(rebuilt.export().unwrap(), expected);
}

/// The agent every record in [`story`] is about.
fn agent() -> AgentId {
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        panic!("the AgentHired sample is not an AgentHired")
    };
    id
}

/// The agent the goal is reassigned to, hired at [`SECOND_HIRE_AT`].
fn other_agent() -> AgentId {
    let Record::GoalReassigned { to, .. } = samples::record(RecordKind::GoalReassigned) else {
        panic!("the GoalReassigned sample is not a GoalReassigned")
    };
    to
}

/// The goal every record in [`story`] is about.
fn goal() -> GoalId {
    let Record::GoalSet {
        goal: Goal { id, .. },
        ..
    } = samples::record(RecordKind::GoalSet)
    else {
        panic!("the GoalSet sample is not a GoalSet")
    };
    id
}

/// The one UTC day [`story`] spends on.
fn day() -> UtcDay {
    Timestamp::from_unix_millis(AT_BASE).utc_day()
}

// ---- runs ---------------------------------------------------------------------------------

/// Write `entries` into a fresh journal under `dir`, emitting a checkpoint every
/// `checkpoint_every` appends.
fn write(dir: &Path, entries: &[Entry], checkpoint_every: u64) {
    let (mut w, _) = open(
        dir,
        JournalOptions {
            fsync: Fsync::None,
            rotate_at: 4096,
            checkpoint_every,
        },
    )
    .unwrap();
    for entry in entries {
        w.append(entry).unwrap();
    }
}

/// What one replay produced: the report, the three row reads as JSON, `spend_views`, and the
/// journal those rows were built from — every row is resolved against **its own** log.
struct Reading {
    report: ReplayReport,
    rows: Value,
    spend: SpendViews,
    log: BTreeMap<Lsn, Entry>,
}

fn replayed(dir: &Path) -> Reading {
    let (reader, _) = Reader::open(dir).unwrap();
    let mut p = Projections::in_memory().unwrap();
    let report = replay(&reader, &mut p).unwrap();
    let log = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();
    Reading {
        report,
        rows: serde_json::json!({
            "agents": p.agents().unwrap(),
            "turns": p.turns().unwrap(),
            "goals": p.goals().unwrap(),
        }),
        // `SpendViews` derives no `Serialize`, so it stays out of the sweep — it is three
        // `Spend`s and holds no position at any depth, so it is compared whole.
        spend: p.spend_views(agent(), goal(), day()).unwrap(),
        log,
    }
}

/// Replay `entries` twice — without checkpoints, then with one every [`EVERY`] appends.
fn both(entries: &[Entry]) -> (Reading, Reading) {
    let plain = tempfile::tempdir().unwrap();
    write(plain.path(), entries, 0);
    let checkpointed = tempfile::tempdir().unwrap();
    write(checkpointed.path(), entries, EVERY);
    (replayed(plain.path()), replayed(checkpointed.path()))
}

// ---- the property -------------------------------------------------------------------------

/// Is this JSON key one a journal position is stored under? `Lsn` is `#[serde(transparent)]`
/// over `u64`, so a position is an anonymous number on the wire and only the key names it.
///
/// **The shape of the key, never a list of fields.** `lsn_issued`, `last_result.lsn` and a field
/// nobody has written yet are all covered by the same check; an enumeration would cover only what
/// someone remembered on the day.
fn names_a_position(key: &str) -> bool {
    key == "lsn" || key.starts_with("lsn_") || key.ends_with("_lsn")
}

/// Replace every stored position in `v` with `Null`, collecting `(path, what the frame at that
/// position holds in this run's own log)`.
///
/// Resolution is the assertion: a position that names no frame in its own journal is a row
/// pointing at nothing, and this panics rather than recording it. `Lsn::ZERO` is the crate's
/// documented sentinel for "before any record" and resolves to `None`, which is still compared
/// between the runs — a mutation that put a real position where `ZERO` was still shows up.
fn resolve_positions(
    v: &mut Value,
    log: &BTreeMap<Lsn, Entry>,
    path: &str,
    out: &mut Vec<(String, Option<Record>)>,
) {
    match v {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let here = format!("{path}.{key}");
                if names_a_position(key) {
                    if matches!(
                        key.as_str(),
                        "session_lsn"
                            | "spawn_lsn"
                            | "end_lsn"
                            | "confirmation_lsn"
                            | "previous_spawn_lsn"
                            | "previous_end_lsn"
                            | "confirmed_session_lsn"
                    ) && child.is_null()
                    {
                        continue;
                    }
                    let raw = child
                        .as_u64()
                        .unwrap_or_else(|| panic!("{here} is not a number: {child}"));
                    let lsn = Lsn(raw);
                    let record = if lsn == Lsn::ZERO {
                        None
                    } else {
                        Some(
                            log.get(&lsn)
                                .unwrap_or_else(|| {
                                    panic!("{here} is {lsn}, which no frame in its own log holds")
                                })
                                .record
                                .clone(),
                        )
                    };
                    out.push((here, record));
                    *child = Value::Null;
                } else {
                    resolve_positions(child, log, &here, out);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                resolve_positions(child, log, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

// ---- the tests ----------------------------------------------------------------------------

#[test]
fn nullable_position_resolution_keeps_absence_zero_and_sources_distinct() {
    let source = samples::record(RecordKind::SessionLearned);
    let at = Lsn(7);
    let log = BTreeMap::from([(
        at,
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record: source.clone(),
        },
    )]);
    let resolve = |key: &str, position: Value| {
        let mut row = serde_json::json!({});
        row[key] = position;
        let mut sources = Vec::new();
        resolve_positions(&mut row, &log, "", &mut sources);
        (row, sources)
    };
    let absent = resolve("session_lsn", serde_json::to_value(None::<Lsn>).unwrap());
    let zero = resolve(
        "session_lsn",
        serde_json::to_value(Some(Lsn::ZERO)).unwrap(),
    );
    let present = resolve("session_lsn", serde_json::to_value(Some(at)).unwrap());
    assert_eq!(absent.0, zero.0);
    assert_eq!(zero.0, present.0);
    assert!(absent.1.is_empty());
    assert_eq!(zero.1, vec![(".session_lsn".into(), None)]);
    assert_eq!(
        present.1,
        vec![(".session_lsn".into(), Some(source.clone()))]
    );
    assert_ne!(absent, zero);
    assert_ne!(zero, present);
    assert_ne!(absent, present);

    for key in [
        "lsn",
        "lsn_issued",
        "hired_lsn",
        "installed_lsn",
        "future_lsn",
    ] {
        assert!(
            std::panic::catch_unwind(|| resolve(key, Value::Null)).is_err(),
            "{key} accepted mandatory null"
        );
        let (_, sources) = resolve(key, serde_json::to_value(at).unwrap());
        assert_eq!(sources, vec![(format!(".{key}"), Some(source.clone()))]);
    }
    for malformed in [
        serde_json::json!("not a position"),
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!(true),
        serde_json::json!({}),
        serde_json::json!(999),
    ] {
        assert!(
            std::panic::catch_unwind(|| resolve("session_lsn", malformed)).is_err(),
            "accepted a malformed or dangling optional position"
        );
    }
}

#[test]
fn checkpointing_changes_no_projection() {
    let entries = story();
    let (plain, with) = both(&entries);

    // The discriminator, first. Without it every assertion below holds for a writer that emits
    // nothing at all, because then the two logs are the same log.
    assert!(
        with.report.applied > plain.report.applied,
        "the checkpointed log holds no extra records: {} vs {}",
        with.report.applied,
        plain.report.applied
    );
    assert_eq!(plain.report.applied, entries.len() as u64);
    assert_eq!(
        with.report.applied,
        plain.report.applied + entries.len() as u64 / EVERY
    );

    // `spend_views` holds no position and is compared whole.
    assert_eq!(plain.spend, with.spend);
    assert_ne!(
        plain.spend,
        SpendViews::default(),
        "the story priced nothing, so the comparison above is vacuous"
    );

    let mut plain_rows = plain.rows.clone();
    let mut plain_at = Vec::new();
    resolve_positions(&mut plain_rows, &plain.log, "", &mut plain_at);
    let mut cp_rows = with.rows.clone();
    let mut cp_at = Vec::new();
    resolve_positions(&mut cp_rows, &with.log, "", &mut cp_at);

    // Non-vacuity, in both directions: rows to compare at all, and at least one position for the
    // property to be about — a sweep that matched nothing would otherwise pass silently.
    for read in ["agents", "turns", "goals"] {
        assert!(
            !plain.rows[read].as_array().unwrap().is_empty(),
            "the story left {read} empty: {}",
            plain.rows
        );
    }
    assert!(
        !plain_at.is_empty(),
        "no stored position was found, so the resolution property asserts nothing"
    );

    // Every stored position resolves to the same record in its own log — same paths, same
    // records — and everything else is equal. Nothing is normalised: a position is *replaced* by
    // what it resolves to, so the field is still asserted, and asserted harder.
    assert_eq!(plain_at, cp_at);
    assert_eq!(plain_rows, cp_rows);
}

/// The control on the test above: the comparison must be able to fail. One extra agent in the
/// second log, and the same rows report a difference.
///
/// Without it, `checkpointing_changes_no_projection` would hold just as well against a
/// comparison that cannot discriminate — a read that always answers empty, or a sweep that
/// blanked everything it walked.
#[test]
fn the_row_comparison_can_fail() {
    let entries = story();
    let mut extra = entries.clone();
    let mut third = samples::record(RecordKind::AgentHired);
    let Record::AgentHired { id, .. } = &mut third else {
        panic!("the AgentHired sample is not an AgentHired")
    };
    *id = AgentId::from_parts(9, 9);
    assert_ne!(*id, agent());
    assert_ne!(*id, other_agent());
    extra.push(Entry {
        at: Timestamp::from_unix_millis(AT_BASE + 100),
        record: third,
    });

    let plain = tempfile::tempdir().unwrap();
    write(plain.path(), &entries, 0);
    let longer = tempfile::tempdir().unwrap();
    write(longer.path(), &extra, EVERY);

    assert_ne!(
        replayed(plain.path()).rows,
        replayed(longer.path()).rows,
        "the row comparison cannot tell two different projections apart"
    );
}

/// Every kind in the contract is either written by the story or carries a stated reason for not
/// being, and no two records claim the same place.
///
/// [`placement`]'s match has no wildcard, so a **new** kind is a compile error. This is the other
/// direction: that the placements actually turn into records, and that a kind quietly re-labelled
/// `Out` shows up as a shrinking corpus rather than as nothing at all.
#[test]
fn the_story_covers_the_whole_corpus() {
    let entries = story();
    let mut written: Vec<RecordKind> = entries.iter().map(|e| e.record.kind()).collect();
    written.sort_unstable();
    written.dedup();

    let mut places = Vec::new();
    let mut out: Vec<(RecordKind, &str)> = Vec::new();
    for kind in RecordKind::ALL {
        match placement(kind) {
            Placement::At(n) => {
                assert!(
                    written.contains(&kind),
                    "{kind:?} is placed at {n} but the story does not write it"
                );
                places.push(n);
            }
            Placement::Out(reason) => {
                assert!(
                    !reason.is_empty(),
                    "{kind:?} is out of the story with no reason"
                );
                assert!(
                    !written.contains(&kind),
                    "{kind:?} is marked out of the story but the story writes it"
                );
                out.push((kind, reason));
            }
        }
    }
    places.push(SECOND_HIRE_AT);
    let before_dedup = places.len();
    places.sort_unstable();
    places.dedup();
    assert_eq!(
        places.len(),
        before_dedup,
        "two records claim the same place in the story"
    );
    assert_eq!(
        written.len() + out.len(),
        RecordKind::ALL.len(),
        "kinds neither written nor excused: {out:?}"
    );
    // The excused set is pinned, not merely non-empty: a kind may legitimately move to `Out`,
    // and when one does this fails, which is the visit that arm exists to force. Every kind but
    // one is written; the exception is the one whose whole point is that the
    // *writer* adds it.
    assert_eq!(
        out.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        [RecordKind::Checkpoint],
        "the set of kinds excused from the story changed"
    );
    assert_eq!(written.len(), RecordKind::ALL.len() - 1);
}

/// `append_witnessed` returns the `crc32c` of **the caller's own frame**, and keeps doing so on
/// the append that also writes a checkpoint.
///
/// That witness is what the projection stores beside `last_applied_lsn`, and production runs `checkpoint_every = 10_000` — so on every ten-thousandth append the
/// daemon would otherwise store a checkpoint's checksum against a record that is not it, and
/// nothing in the crate would say so. The `buf` the witness is read out of is cleared and
/// refilled by `emit_checkpoint`; only statement order keeps it the caller's, and only this test
/// says so.
#[test]
fn a_witness_stays_the_caller_s_own_frame_across_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, _) = open(
        dir.path(),
        JournalOptions {
            fsync: Fsync::None,
            rotate_at: 1 << 20,
            checkpoint_every: 2,
        },
    )
    .unwrap();
    let entries = story();
    let witnessed: Vec<(Lsn, u32)> = entries
        .iter()
        .take(6)
        .map(|entry| w.append_witnessed(entry).unwrap())
        .collect();
    drop(w);

    let bytes = std::fs::read(dir.path().join("000001.log")).unwrap();
    for (i, (lsn, crc)) in witnessed.iter().enumerate() {
        let at = usize::try_from(lsn.offset()).unwrap();
        let header: [u8; frame::HEADER_LEN] = bytes[at..at + frame::HEADER_LEN].try_into().unwrap();
        assert_eq!(
            frame::Header::parse(&header).crc,
            *crc,
            "append {i} was handed a checksum that is not its own frame's"
        );
        let decoded = frame::decode(&bytes[at..]).unwrap();
        let back: Entry = serde_json::from_slice(decoded.payload).unwrap();
        assert_eq!(
            back.record.kind(),
            entries[i].record.kind(),
            "append {i}'s lsn does not name its own record"
        );
    }

    // The control: this journal really does hold the checkpoints whose frames the witness must
    // not have been taken from. Without it every assertion above holds trivially on a writer
    // that emitted none.
    let (reader, _) = Reader::open(dir.path()).unwrap();
    let checkpoints = reader
        .iter_from(Lsn::ZERO)
        .unwrap()
        .filter(|item| {
            matches!(
                item.as_ref().unwrap().1.record.kind(),
                RecordKind::Checkpoint
            )
        })
        .count();
    assert_eq!(
        checkpoints, 3,
        "one per two appends, and the story writes none of its own"
    );
}
