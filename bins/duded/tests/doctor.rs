//! `system.doctor` carries the supervisor's adoption-key and session-recovery diagnostics over the
//! daemon's own socket route.
//!
//! The same durable open turn is read on two daemon runs. The first host can interpret the key it
//! stamped; the second cannot read a boot identity, so the response changes in exactly the two
//! places allowed: the run's boot report and the per-turn mark. This is a live daemon test,
//! not a handler unit test, so that the route itself is exercised and not only the layer beneath it.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_journal::Projections;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::system::{AgentHealth, DoctorResult, OpenTurnHealth};
use dudes_proto::rpc::{JSONRPC, Method, Request, RequestId};
use dudes_proto::{Entry, ExitStatus, Record, TurnOutcome};
use dudes_providers::launch::LaunchSpec;
use dudes_providers::stream::ScopedUsage;
use dudes_rpc::auth::{PeerClassifier, PeerProvenance};
use dudes_rpc::{Client, Outbound, Subscribers};
use dudes_supervisor::testing::{FakeClock, FakeHost, FakeRng, Harness};
use dudes_supervisor::{HostBoot, Input, KeyHealth, Supervisor};
use dudes_types::{
    AgentId, GoalId, Lsn, Mode, Provenance, SessionId, Timestamp, Tokens, TurnId, Usage, Usd,
};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, hire, seams, set_goal};

const TEST_CREDENTIAL: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

fn test_credential() -> OperatorCredential {
    OperatorCredential::new(TEST_CREDENTIAL).expect("the fixture credential")
}

/// The credential this daemon minted, read the way an operator client does.
fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted a credential");
    OperatorCredential::new(text).expect("the credential is well formed")
}

/// Call the live route and decode its published result.
fn doctor(settings: &duded::Settings) -> DoctorResult {
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");

    let refused = client
        .call(Method::SystemDoctor, None)
        .expect("an unproven call is answered");
    assert!(
        refused.result.is_none() && refused.error.is_some(),
        "system.doctor is Operator-only: {refused:?}"
    );

    let response = client
        .call_as_operator(Method::SystemDoctor, None, credential_of(settings))
        .expect("system.doctor is answered");
    assert!(
        response.error.is_none(),
        "the operator's system.doctor call succeeds: {:?}",
        response.error
    );
    serde_json::from_value(response.result.expect("a doctor result"))
        .expect("the existing DoctorResult wire shape")
}

/// `annotate` answers a sign-in row `Unknown`/"still checking" once its own
/// `Timing::DAEMON.wait` elapses while the real status command is still scheduling, which under
/// machine load it can do well inside `Timing::DAEMON.deadline`, the bound the command is actually
/// killed by. A non-signed-in answer is never cached (`SignInCheck::annotate` starts a fresh check
/// only once its slot is idle), so a call placed immediately after "still checking" rejoins the
/// one check already running instead of racing a new one; a call placed after a sleep would not,
/// because the in-flight check would have already finished and gone idle, restarting the same race
/// from zero. Bound the whole wait by `Timing::DAEMON`'s own fields, not an arbitrary number, with
/// one extra `wait` of slack so the call that finally observes the finish has a full cycle to do it
/// in.
fn doctor_after_sign_in_settles(settings: &duded::Settings) -> DoctorResult {
    use dudes_proto::rpc::system::SignIn;

    let timing = duded::sign_in::Timing::DAEMON;
    let deadline = Instant::now() + timing.deadline + timing.wait;
    loop {
        let result = doctor(settings);
        let still_checking = result
            .providers
            .first()
            .is_some_and(|probe| probe.sign_in == Some(SignIn::Unknown));
        if !still_checking || Instant::now() >= deadline {
            return result;
        }
    }
}

/// Exactly one row describes `turn`; a missing or duplicate row is not a usable diagnostic.
fn row(result: &DoctorResult, turn: TurnId) -> &OpenTurnHealth {
    let rows: Vec<_> = result
        .open_turns
        .iter()
        .filter(|row| row.turn == turn)
        .collect();
    assert_eq!(rows.len(), 1, "one row for the open turn: {result:?}");
    rows[0]
}

fn agent_row(result: &DoctorResult, agent: AgentId) -> &AgentHealth {
    result
        .agents
        .iter()
        .find(|row| row.agent == agent)
        .unwrap_or_else(|| panic!("one health row for {agent}: {result:?}"))
}

fn issued_session(records: &[Record], turn: TurnId) -> SessionId {
    records
        .iter()
        .find_map(|record| match record {
            Record::TurnIssued {
                turn: candidate,
                session,
                ..
            } if *candidate == turn => session.clone(),
            _ => None,
        })
        .expect("the turn issued with a session")
}

fn issued_entry_session(entries: &[Entry], turn: TurnId) -> SessionId {
    entries
        .iter()
        .find_map(|entry| match &entry.record {
            Record::TurnIssued {
                turn: candidate,
                session,
                ..
            } if *candidate == turn => session.clone(),
            _ => None,
        })
        .expect("the turn issued with a session")
}

fn turn_after(fixture: &Fixture, agent: AgentId, previous: TurnId) -> TurnId {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut saw_previous = false;
        for record in fixture.records() {
            if let Record::TurnIssued {
                agent: owner, turn, ..
            } = record
                && owner == agent
            {
                if saw_previous {
                    return turn;
                }
                saw_previous = turn == previous;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no later TurnIssued for {agent} after {previous}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn reported_usage() -> Usage {
    Usage {
        input: Tokens::new(3),
        output: Tokens::new(5),
        cache_read: Tokens::new(2),
        cache_creation: Tokens::new(1),
        usd: Some(Usd::from_dollars(1)),
    }
}

fn end(fixture: &Fixture, host: &SharedHost, turn: TurnId, usage: Option<Usage>) {
    common::end_turn_with_native_payload(
        fixture,
        host,
        Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: usage.map(ScopedUsage::Turn),
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: Some("not yet".to_owned()),
        },
    );
}

fn announced(spec: &LaunchSpec) -> (&'static str, String) {
    let argv: Vec<_> = spec
        .argv
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    for (flag, kind) in [("--session-id", "first"), ("--resume", "resume")] {
        if let Some(at) = argv.iter().position(|part| part == flag) {
            return (
                kind,
                argv.get(at + 1)
                    .unwrap_or_else(|| panic!("{flag} has an operand: {argv:?}"))
                    .clone(),
            );
        }
    }
    panic!("a managed launch announces one session: {argv:?}")
}

fn await_launches(host: &SharedHost, count: usize) -> Vec<LaunchSpec> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let launches = host.spawned();
        if launches.len() >= count {
            return launches;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {count} launches arrived",
            launches.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn replayed_sessions(
    entries: &[(Lsn, Entry)],
    agent: AgentId,
) -> (Option<SessionId>, Option<SessionId>) {
    let mut supervisor = Supervisor::new(
        Harness::config(),
        FakeClock::frozen(1_787_394_600_000),
        FakeRng::new(),
    );
    let mut projections = Projections::in_memory().expect("fresh redb projection");
    for (lsn, entry) in entries {
        let _ = supervisor.handle(Input::Replayed {
            lsn: *lsn,
            entry: entry.clone(),
        });
        projections
            .apply(*lsn, 1, entry)
            .expect("the entry folds into redb");
    }
    (
        supervisor.world().agents[&agent].session.clone(),
        projections
            .agent(agent)
            .expect("read agent")
            .expect("agent row")
            .session,
    )
}

fn replayed_session_states(
    entries: &[(Lsn, Entry)],
    agent: AgentId,
    turn: TurnId,
) -> ((Option<SessionId>, bool), (Option<SessionId>, bool)) {
    let mut supervisor = Supervisor::new(
        Harness::config(),
        FakeClock::frozen(1_787_394_600_000),
        FakeRng::new(),
    );
    let mut projections = Projections::in_memory().expect("fresh redb projection");
    for (lsn, entry) in entries {
        let _ = supervisor.handle(Input::Replayed {
            lsn: *lsn,
            entry: entry.clone(),
        });
        projections.apply(*lsn, 1, entry).expect("fold entry");
    }
    let world_turn = &supervisor.world().turns[&turn];
    let projected_turn = projections
        .turn(turn)
        .expect("read turn")
        .expect("turn row");
    (
        (
            supervisor.world().agents[&agent].session.clone(),
            world_turn.session_confirmed,
        ),
        (
            projections
                .agent(agent)
                .expect("read agent")
                .expect("agent row")
                .session,
            projected_turn.session_confirmed,
        ),
    )
}

fn push_record(journal: &mut Vec<Entry>, record: Record) {
    journal.push(Entry {
        at: Timestamp::from_unix_millis(i64::try_from(journal.len()).unwrap()),
        record,
    });
}

#[derive(Clone, Copy)]
enum ManagedRecord<'a> {
    Issued {
        goal: GoalId,
        session: &'a SessionId,
    },
    Spawned {
        pid: u32,
        session: &'a SessionId,
        reissued: bool,
    },
    Learned(&'a SessionId),
    Ended(&'a SessionId),
}

fn managed_record(agent: AgentId, turn: TurnId, record: ManagedRecord<'_>) -> Record {
    match record {
        ManagedRecord::Issued { goal, session } => Record::TurnIssued {
            agent,
            turn,
            goal,
            task: "continue".to_owned(),
            session: Some(session.clone()),
            mode: Mode::Managed,
            model_selection: None,
            lsn_of_decision: Lsn::ZERO,
            answers: Vec::new(),
        },
        ManagedRecord::Spawned {
            pid,
            session,
            reissued,
        } => Record::TurnSpawned {
            agent,
            turn,
            pid,
            start_time: 900,
            boot: None,
            session: Some(session.clone()),
            reissued,
            mcp_grant: None,
        },
        ManagedRecord::Learned(session) => Record::SessionLearned {
            agent,
            turn,
            session: session.clone(),
        },
        ManagedRecord::Ended(session) => Record::TurnEnded {
            final_reply: None,
            elapsed_ms: None,
            session_usage: None,
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: ExitStatus::UNKNOWN,
            usage: None,
            model_usage: BTreeMap::new(),
            last_message: None,
            session: Some(session.clone()),
            outcome: Some(TurnOutcome::Completed),
        },
    }
}

fn push_managed(journal: &mut Vec<Entry>, agent: AgentId, turn: TurnId, record: ManagedRecord<'_>) {
    push_record(journal, managed_record(agent, turn, record));
}

struct SessionReplayTrace {
    journal: Vec<Entry>,
    agent: AgentId,
    first_trigger: usize,
    second_trigger: usize,
    delayed: TurnId,
    stale_cleared: usize,
    current_confirmed: usize,
    next: TurnId,
    current_session: SessionId,
}

fn append_invalidation_records(
    journal: &mut Vec<Entry>,
    agent: AgentId,
    goal: GoalId,
    confirmed: &SessionId,
) -> (usize, usize) {
    let reissued = TurnId::from_parts(100, 1);
    let resumed = TurnId::from_parts(100, 2);
    let fresh = SessionId::new("fresh-after-reissue");
    push_managed(
        journal,
        agent,
        reissued,
        ManagedRecord::Issued {
            goal,
            session: confirmed,
        },
    );
    push_managed(
        journal,
        agent,
        reissued,
        ManagedRecord::Spawned {
            pid: 2000,
            session: &fresh,
            reissued: true,
        },
    );
    let first_trigger = journal.len();
    push_managed(journal, agent, reissued, ManagedRecord::Learned(&fresh));
    push_managed(journal, agent, reissued, ManagedRecord::Ended(&fresh));
    push_managed(
        journal,
        agent,
        resumed,
        ManagedRecord::Issued {
            goal,
            session: &fresh,
        },
    );
    push_managed(
        journal,
        agent,
        resumed,
        ManagedRecord::Spawned {
            pid: 2001,
            session: &fresh,
            reissued: false,
        },
    );
    push_managed(journal, agent, resumed, ManagedRecord::Ended(&fresh));
    (first_trigger, journal.len())
}

fn append_delayed_confirmation(
    journal: &mut Vec<Entry>,
    agent: AgentId,
    goal: GoalId,
) -> (TurnId, usize, usize, TurnId, SessionId) {
    let first_again = TurnId::from_parts(100, 3);
    let delayed = TurnId::from_parts(100, 4);
    let next = TurnId::from_parts(100, 5);
    let base = SessionId::new("base-before-delayed-spawn");
    let announced = SessionId::new("announced-by-current-reissue");
    let current = SessionId::new("assigned-by-current-reissue");
    for (turn, record) in [
        (
            first_again,
            ManagedRecord::Issued {
                goal,
                session: &base,
            },
        ),
        (
            first_again,
            ManagedRecord::Spawned {
                pid: 2002,
                session: &base,
                reissued: false,
            },
        ),
        (first_again, ManagedRecord::Learned(&base)),
        (first_again, ManagedRecord::Ended(&base)),
        (
            delayed,
            ManagedRecord::Issued {
                goal,
                session: &base,
            },
        ),
        // This confirmation belongs to the previous launch attempt of the same TurnId.
        (delayed, ManagedRecord::Learned(&base)),
        (
            delayed,
            ManagedRecord::Spawned {
                pid: 2003,
                session: &announced,
                reissued: true,
            },
        ),
    ] {
        push_managed(journal, agent, turn, record);
    }
    let stale_cleared = journal.len();
    push_managed(journal, agent, delayed, ManagedRecord::Learned(&current));
    let current_confirmed = journal.len();
    for (turn, record) in [
        (delayed, ManagedRecord::Ended(&current)),
        (
            next,
            ManagedRecord::Issued {
                goal,
                session: &current,
            },
        ),
    ] {
        push_managed(journal, agent, turn, record);
    }
    (delayed, stale_cleared, current_confirmed, next, current)
}

fn session_replay_trace() -> SessionReplayTrace {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["false"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let confirmed = issued_session(&harness.records().cloned().collect::<Vec<_>>(), first);
    harness.drive(Input::SessionLearned {
        turn: first,
        session: confirmed.clone(),
    });
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn: first,
        exit: ExitStatus::UNKNOWN,
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });

    let mut journal = harness.journal.clone();
    let (first_trigger, second_trigger) =
        append_invalidation_records(&mut journal, agent, goal, &confirmed);
    let (delayed, stale_cleared, current_confirmed, next, current_session) =
        append_delayed_confirmation(&mut journal, agent, goal);
    SessionReplayTrace {
        journal,
        agent,
        first_trigger,
        second_trigger,
        delayed,
        stale_cleared,
        current_confirmed,
        next,
        current_session,
    }
}

/// A live daemon reports its boot identity and marks only a key this run cannot interpret.
///
/// Removing the `boot::dispatch` registration makes both calls fail with `method not found`.
/// Returning a constant boot or mark fails one of the two arms. The second boot reuses the first
/// run's journal, so the comparison is against a real replayed `TurnSpawned` key rather than a
/// test-only `KeyHealth` fixture.
#[test]
fn live_doctor_reports_boot_identity_and_the_uninterpretable_open_turn() {
    let fixture = Fixture::new();
    let settings = fixture.settings();

    let daemon = boot(
        &settings,
        seams(
            SharedHost::with(FakeHost::boot_relative("boot-a")),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot with an identity");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    set_goal(&running.handle, goal_params(agent, &["/usr/bin/true"]));
    let turn = fixture.await_turn();

    let readable = doctor(&settings);
    assert_eq!(
        readable.boot,
        Some(HostBoot::Read("boot-a".to_owned())),
        "the first diagnostic line names this run's boot identity"
    );
    assert!(
        !row(&readable, turn).key_uninterpretable,
        "the key stamped by this boot is interpretable, so there is no second-line mark"
    );
    running.stop(ExitCause::Ended);
    let replayed_lsn = fixture
        .entries()
        .last()
        .map(|(lsn, _)| *lsn)
        .expect("the first run left a durable journal cursor");
    assert_ne!(
        replayed_lsn,
        Lsn::ZERO,
        "control: restart has non-empty journal state to publish"
    );

    let daemon = boot(
        &settings,
        seams(
            SharedHost::with(FakeHost::boot_unreadable("permission denied")),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("restart without a readable boot identity");
    let mut running = common::start(daemon);

    let unreadable = doctor(&settings);
    assert_eq!(
        unreadable.ping.lsn, replayed_lsn,
        "replayed health and ping name the same latest durable record before any new append"
    );
    assert_ne!(
        unreadable.ping.lsn,
        Lsn::ZERO,
        "a non-empty replay is never published as an empty journal"
    );
    assert_eq!(
        unreadable.boot,
        Some(HostBoot::Unreadable("permission denied".to_owned())),
        "the first diagnostic line preserves why this run has no boot identity"
    );
    assert!(
        row(&unreadable, turn).key_uninterpretable,
        "the second line marks the replayed key this run cannot interpret"
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn an_unconfirmed_resume_falls_back_once_to_first() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::fails()])),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    let goal = set_goal(&running.handle, goal_params(agent, &["/usr/bin/false"]));

    let first = fixture.await_turn();
    let session = issued_session(&fixture.records(), first);
    running.handle.input(Input::SessionLearned {
        turn: first,
        session: session.clone(),
    });
    fixture.await_record("the first confirmation", |record| {
        matches!(record, Record::SessionLearned { turn, .. } if *turn == first).then_some(())
    });
    end(&fixture, &host, first, None);
    let resumed = turn_after(&fixture, agent, first);
    let launches = await_launches(&host, 2);
    assert_eq!(announced(&launches[1]), ("resume", session.to_string()));

    end(&fixture, &host, resumed, Some(reported_usage()));
    let fallback = turn_after(&fixture, agent, resumed);
    let health = doctor(&settings);
    assert_eq!(agent_row(&health, agent).unconfirmed_resume, Some(resumed));

    let launches = await_launches(&host, 3);
    let (kind, fresh) = announced(&launches[2]);
    assert_eq!(kind, "first");
    assert_ne!(fresh, session.as_str());
    assert_eq!(issued_session(&fixture.records(), fallback).as_str(), fresh);

    running.stop(ExitCause::Ended);
    let entries = fixture.entries();
    assert_eq!(replayed_sessions(&entries, agent), (None, None));
    let mut projection = Projections::in_memory().expect("fresh projection");
    for (lsn, entry) in &entries {
        projection.apply(*lsn, 1, entry).expect("fold entry");
    }
    assert_eq!(
        projection.goal(goal).unwrap().unwrap().turns_on_goal,
        3,
        "one establishing hop, then exactly one resume and one fallback hop"
    );
    assert_eq!(
        projection.spend_by_goal(goal).unwrap().usd,
        Usd::from_dollars(1),
        "the resume's reported usage is charged once"
    );
    assert_eq!(
        projection.spend_by_goal(goal).unwrap().tokens,
        Tokens::new(11)
    );
    assert_eq!(
        projection.turn(fallback).unwrap().unwrap().usage,
        None,
        "the fallback is not charged before its own end"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::SessionLearned { .. }))
            .count(),
        1,
        "the absence adds no demotion record or invented confirmation"
    );
}

#[test]
fn a_confirmed_resume_does_not_fall_back() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::fails()])),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    let goal = set_goal(&running.handle, goal_params(agent, &["/usr/bin/false"]));

    let first = fixture.await_turn();
    let session = issued_session(&fixture.records(), first);
    running.handle.input(Input::SessionLearned {
        turn: first,
        session: session.clone(),
    });
    end(&fixture, &host, first, None);
    let resumed = turn_after(&fixture, agent, first);
    running.handle.input(Input::SessionLearned {
        turn: resumed,
        session: session.clone(),
    });
    fixture.await_record("the resumed confirmation", |record| {
        matches!(record, Record::SessionLearned { turn, .. } if *turn == resumed).then_some(())
    });
    end(&fixture, &host, resumed, Some(reported_usage()));
    let next = turn_after(&fixture, agent, resumed);

    assert_eq!(
        agent_row(&doctor(&settings), agent).unconfirmed_resume,
        None
    );
    let launches = await_launches(&host, 3);
    assert_eq!(announced(&launches[2]), ("resume", session.to_string()));
    assert_eq!(issued_session(&fixture.records(), next), session);

    running.stop(ExitCause::Ended);
    let entries = fixture.entries();
    let mut projection = Projections::in_memory().expect("fresh projection");
    for (lsn, entry) in &entries {
        projection.apply(*lsn, 1, entry).expect("fold entry");
    }
    assert_eq!(projection.goal(goal).unwrap().unwrap().turns_on_goal, 3);
    assert_eq!(
        projection.spend_by_goal(goal).unwrap().usd,
        Usd::from_dollars(1)
    );
}

#[test]
fn another_agents_session_neither_binds_nor_clears_resume_recovery() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = seams(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::fails()])),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);

    let folder_a = tempfile::tempdir_in(fixture.state.path()).expect("agent A working directory");
    let folder_b = tempfile::tempdir_in(fixture.state.path()).expect("agent B working directory");
    let agent_a = hire(&running.handle, folder_a.path());
    set_goal(&running.handle, goal_params(agent_a, &["/usr/bin/false"]));
    let a_first = fixture.await_turn();
    let session_a = issued_session(&fixture.records(), a_first);

    let agent_b = hire(&running.handle, folder_b.path());
    set_goal(&running.handle, goal_params(agent_b, &["/usr/bin/false"]));
    let b_first = fixture.await_record("agent B's turn", |record| match record {
        Record::TurnIssued { agent, turn, .. } if *agent == agent_b => Some(*turn),
        _ => None,
    });
    let session_b = issued_session(&fixture.records(), b_first);

    for (turn, session) in [(a_first, session_a.clone()), (b_first, session_b.clone())] {
        running
            .handle
            .input(Input::SessionLearned { turn, session });
    }
    end(&fixture, &host, a_first, None);
    let a_resumed = turn_after(&fixture, agent_a, a_first);
    end(&fixture, &host, b_first, None);
    let b_resumed = turn_after(&fixture, agent_b, b_first);
    end(&fixture, &host, a_resumed, None);
    let a_fallback = turn_after(&fixture, agent_a, a_resumed);
    assert_eq!(
        agent_row(&doctor(&settings), agent_a).unconfirmed_resume,
        Some(a_resumed)
    );

    running.handle.input(Input::SessionLearned {
        turn: a_fallback,
        session: session_b.clone(),
    });
    assert_eq!(
        agent_row(&doctor(&settings), agent_a).unconfirmed_resume,
        Some(a_resumed),
        "another agent's session neither binds nor clears the recovery"
    );
    running.handle.input(Input::SessionLearned {
        turn: b_resumed,
        session: session_b.clone(),
    });
    assert_eq!(
        agent_row(&doctor(&settings), agent_a).unconfirmed_resume,
        Some(a_resumed),
        "agent B's own accepted confirmation does not clear agent A"
    );

    let recovered = issued_session(&fixture.records(), a_fallback);
    running.handle.input(Input::SessionLearned {
        turn: a_fallback,
        session: recovered.clone(),
    });
    assert_eq!(
        agent_row(&doctor(&settings), agent_a).unconfirmed_resume,
        None
    );
    running.stop(ExitCause::Ended);

    let learned_for_a: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::SessionLearned {
                agent,
                turn,
                session,
            } if agent == agent_a && turn == a_fallback => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(learned_for_a, std::slice::from_ref(&recovered));
    assert_ne!(recovered, session_b);
}

#[test]
fn replaying_both_session_triggers_keeps_world_and_redb_equal() {
    let SessionReplayTrace {
        journal,
        agent,
        first_trigger,
        second_trigger,
        delayed,
        stale_cleared,
        current_confirmed,
        next,
        current_session,
    } = session_replay_trace();

    for prefix in [first_trigger, second_trigger] {
        let entries: Vec<_> = journal[..prefix]
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, entry)| (Lsn(u64::try_from(index + 1).unwrap()), entry))
            .collect();
        assert_eq!(
            replayed_sessions(&entries, agent),
            (None, None),
            "both projections clear the same session after each trigger"
        );
    }
    for (prefix, expected) in [
        (stale_cleared, (None, false)),
        (current_confirmed, (Some(current_session.clone()), true)),
    ] {
        let entries: Vec<_> = journal[..prefix]
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, entry)| (Lsn(u64::try_from(index + 1).unwrap()), entry))
            .collect();
        assert_eq!(
            replayed_session_states(&entries, agent, delayed),
            (expected.clone(), expected),
            "both folds distinguish the previous attempt from the current confirmation"
        );
    }
    let entries: Vec<_> = journal
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, entry)| (Lsn(u64::try_from(index + 1).unwrap()), entry))
        .collect();
    assert_eq!(
        replayed_sessions(&entries, agent),
        (Some(current_session.clone()), Some(current_session.clone())),
        "the current attempt's held confirmation follows the reissue carrier in both folds"
    );
    assert_eq!(issued_entry_session(&journal, next), current_session);
}

/// A diagnostic queued behind Stop is answered by the exit drain rather than left hanging.
///
/// The two-phase handle call makes the order deterministic: both jobs are queued on the same
/// sender before `run` can receive either. The bounded receive makes retaining the reply sender
/// without answering a test failure rather than a hung test process.
#[test]
fn shutdown_drain_answers_doctor_queued_after_stop_without_hanging() {
    let fixture = Fixture::new();
    let daemon = boot(
        &fixture.settings(),
        seams(
            SharedHost::with(FakeHost::boot_relative("boot-a")),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let handle = daemon.handle();

    handle.stop(ExitCause::Ended);
    let reply = handle
        .queue_doctor()
        .expect("Doctor is queued behind Stop before the loop starts");
    assert_eq!(
        daemon.run().expect("the daemon drains and exits"),
        ExitCause::Ended
    );

    let result = reply
        .recv_timeout(Duration::from_secs(1))
        .expect("the shutdown drain answers Doctor within the test bound")
        .expect("Doctor computes its diagnostic during the drain");
    assert_eq!(result.health.boot, HostBoot::Read("boot-a".to_owned()));
}

/// `ping.lsn` and the health rows are one loop-owned snapshot even if publication advances before
/// the RPC handler builds its answer.
///
/// The loop side deliberately captures health at lsn 1, then schedules a concurrent publication
/// to lsn 2 before releasing the reply. Reading the shared atomic after `Handle::doctor` returns
/// therefore pairs lsn 2 with stale health and fails this assertion. The production answer must
/// instead carry the lsn captured by the same loop job as the health.
#[test]
fn doctor_does_not_pair_newer_ping_lsn_with_older_health() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let published_lsn = Arc::new(AtomicU64::new(1));
    let (jobs, loop_side) = mpsc::channel();
    let dispatcher = boot::dispatch(
        &settings,
        PeerClassifier::new(
            test_credential(),
            Arc::new(duded::classify::WorldTokens::new()),
        ),
        duded::hooks::Ingress::new(duded::ports::Handle::new(jobs), Arc::new(TestClock::new())),
        &fixture.journal_dir(),
        &Arc::new(Subscribers::new()),
        Arc::clone(&published_lsn),
        Arc::new(dudes_rpc::OperatorAuthoritySource::from_replayed_agents(
            false,
        )),
    );

    let call = thread::spawn(move || {
        let request = Request {
            jsonrpc: JSONRPC.to_owned(),
            id: RequestId::Number(1),
            method: Method::SystemDoctor.name().to_owned(),
            params: None,
            v: None,
            cred: Some(test_credential()),
        };
        dispatcher.dispatch(
            &request,
            PeerProvenance::Unresolvable,
            &Arc::new(Outbound::sink()),
        )
    });

    let duded::ports::Job::Doctor { reply } = loop_side
        .recv_timeout(Duration::from_secs(1))
        .expect("the handler queues one loop-owned diagnostic read")
    else {
        panic!("system.doctor queued a non-diagnostic job");
    };
    published_lsn.store(2, Ordering::SeqCst);
    reply
        .send(Ok(duded::ports::DoctorSnapshot {
            journal_stopped: None,
            lsn: Lsn(1),
            health: KeyHealth {
                boot: HostBoot::Read("boot-at-lsn-1".to_owned()),
                turns: Vec::new(),
                issued_not_spawned_turns: Vec::new(),
            },
            agents: Vec::new(),
            busy: std::collections::BTreeSet::new(),
        }))
        .expect("the handler is waiting for the loop snapshot");

    let response = call.join().expect("the handler thread");
    assert!(response.error.is_none(), "doctor succeeds: {response:?}");
    let result: DoctorResult = serde_json::from_value(response.result.expect("a doctor result"))
        .expect("the existing DoctorResult wire shape");
    assert_eq!(
        result.ping.lsn,
        Lsn(1),
        "the ping lsn comes from the same loop snapshot as boot-at-lsn-1"
    );
    assert_eq!(
        published_lsn.load(Ordering::SeqCst),
        2,
        "control: publication really advanced before the handler built its answer"
    );
}

/// An executable file at `path`.
fn install_runnable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, "#!/bin/sh\n").expect("the program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("make it executable");
}

/// A daemon whose one provider is Claude configured to run the bare name `prog`, searched on
/// `path`.
fn prog_daemon(fixture: &Fixture, path: &std::ffi::OsStr) -> (duded::Settings, common::Running) {
    prog_daemon_with(fixture, path, Arc::new(ScriptedRunner::default()))
}

/// [`prog_daemon`] on a chosen one-shot runner. Beside `PATH`, the daemon's environment holds a
/// variable the Claude adapter scrubs from a managed turn, `CLAUDECODE`, and one it keeps.
fn prog_daemon_with<P: duded::predicate::PredicateRunner>(
    fixture: &Fixture,
    path: &std::ffi::OsStr,
    runner: Arc<P>,
) -> (duded::Settings, common::Running) {
    use dudes_providers::adapter::Adapter;
    use dudes_providers::claude::ClaudeAdapter;
    use dudes_providers::claude::config::ClaudeConfig;
    use dudes_providers::launch::EnvVar;

    let mut settings = fixture.settings();
    settings.daemon_env = vec![
        EnvVar::new("PATH", path),
        EnvVar::new("CLAUDECODE", "1"),
        EnvVar::new("KEEP", "kept"),
    ];
    let seams = duded::Seams {
        adapters: dudes_providers::registry::Adapters::new([Box::new(ClaudeAdapter::new(
            ClaudeConfig {
                command: "prog".into(),
                ..ClaudeConfig::default()
            },
        )) as Box<dyn Adapter>]),
        ..seams(
            SharedHost::new(),
            runner,
            Arc::new(TestClock::new()),
            Witness::default(),
        )
    };
    let running = common::start(boot(&settings, seams).expect("boot"));
    (settings, running)
}

/// The shipped one-shot runner, as `main.rs` wires it, for the status command a doctor call runs.
struct RealRunner(dudes_host::OneShot);

impl duded::predicate::PredicateRunner for RealRunner {
    fn run(
        &self,
        _spec: &LaunchSpec,
        _timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        Ok(match self.0.run_discarding_output(spec, timeout)? {
            dudes_host::OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => duded::predicate::PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            dudes_host::OneShotOutcome::Swept => duded::predicate::PredicateOutcome::Swept,
        })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

/// `prog` as a stand-in engine that records each run's arguments, `CLAUDECODE` and `KEEP` in
/// `runs`, and exits `code`.
fn install_engine(path: &std::path::Path, runs: &std::path::Path, code: u8) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\necho \"$* ${{CLAUDECODE-unset}} ${{KEEP-unset}}\" >> '{}'\nexit {code}\n",
            runs.display()
        ),
    )
    .expect("the program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("make it executable");
}

/// Over the wire: the doctor row carries the status command's answer, asked with the
/// adapter's own arguments in the environment the adapter gives a managed turn.
#[test]
fn doctor_reports_an_engines_sign_in_from_its_status_command() {
    use dudes_proto::rpc::system::SignIn;

    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    let runs = fixture.state.path().join("runs");
    install_engine(&bin.join("prog"), &runs, 1);
    let (settings, mut running) = prog_daemon_with(
        &fixture,
        bin.as_os_str(),
        Arc::new(RealRunner(dudes_host::OneShot::new())),
    );
    let providers = doctor_after_sign_in_settles(&settings).providers;
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].sign_in, Some(SignIn::SignedOut));
    assert_eq!(providers[0].detail, None);
    assert_eq!(
        std::fs::read_to_string(&runs).expect("the status command ran"),
        "auth status unset kept\n",
        "the Claude scrub removed CLAUDECODE and kept the rest"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// No status command starts while a turn of that engine is open.
#[test]
fn doctor_starts_no_sign_in_check_while_a_turn_is_open() {
    use dudes_proto::rpc::system::SignIn;

    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    let runs = fixture.state.path().join("runs");
    install_engine(&bin.join("prog"), &runs, 0);
    let (settings, mut running) = prog_daemon_with(
        &fixture,
        bin.as_os_str(),
        Arc::new(RealRunner(dudes_host::OneShot::new())),
    );
    let agent = hire(&running.handle, fixture.state.path());
    set_goal(&running.handle, goal_params(agent, &["/usr/bin/true"]));
    fixture.await_turn();

    let providers = doctor(&settings).providers;
    assert_eq!(providers[0].sign_in, Some(SignIn::Unknown));
    assert_eq!(providers[0].detail.as_deref(), Some("a turn is running"));
    assert!(!runs.exists(), "the status command did not run");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn doctor_reports_each_configured_providers_check() {
    use dudes_proto::rpc::system::{ProviderProbe, SignIn};
    use dudes_types::Provider;

    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_runnable(&bin.join("prog"));
    let (settings, mut running) = prog_daemon(&fixture, bin.as_os_str());
    assert_eq!(
        doctor(&settings).providers,
        vec![ProviderProbe {
            login_home: None,
            provider: Provider::Claude,
            command: Some(bin.join("prog")),
            version: None,
            ok: true,
            detail: Some("the status command could not start".to_owned()),
            sign_in: Some(SignIn::Unknown),
        }],
        "the scripted runner starts no status command, which reads as a launch failure"
    );
    std::fs::remove_file(bin.join("prog")).expect("remove prog");
    thread::sleep(Duration::from_millis(1_150));
    assert_eq!(
        doctor(&settings).providers,
        vec![ProviderProbe {
            login_home: None,
            provider: Provider::Claude,
            command: None,
            version: None,
            ok: false,
            detail: None,
            sign_in: None,
        }],
        "a fresh check sees the program gone, and a missing program is not checked"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    // APFS refuses a file name that is not UTF-8, so only Linux can hold one on its PATH.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let fixture = Fixture::new();
        let odd = fixture
            .state
            .path()
            .join(std::ffi::OsStr::from_bytes(b"bin-\xff"));
        install_runnable(&odd.join("prog"));
        let (settings, mut running) = prog_daemon(&fixture, odd.as_os_str());
        assert_eq!(
            doctor(&settings).providers,
            vec![ProviderProbe {
                login_home: None,
                provider: Provider::Claude,
                command: None,
                version: None,
                ok: true,
                detail: Some("the program was found at a path that is not valid UTF-8".to_owned()),
                sign_in: None,
            }],
            "one path that is not UTF-8 leaves its row without a command, and the reply stands"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn doctor_provider_check_runs_off_the_loop_thread() {
    use dudes_proto::rpc::system::ProviderProbe;
    use dudes_types::Provider;

    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let check = daemon.provider_check();
    let mut running = common::start(daemon);
    let workspace = fixture.state.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    let agent = hire(&running.handle, &workspace);

    check.hold_next();
    let asking = settings.clone();
    let (replied, reply) = mpsc::channel();
    thread::spawn(move || {
        let started = Instant::now();
        let result = doctor(&asking);
        let _ = replied.send((result, started.elapsed()));
    });
    assert!(
        check.wait_held(Duration::from_secs(5)),
        "the doctor call started a check"
    );
    let handle = running.handle.clone();
    let pause_call = thread::spawn(move || handle.ask(duded::ports::Ask::Pause(agent)));
    let deadline = Instant::now() + Duration::from_secs(3);
    let paused = loop {
        let found = fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Pause { agent: named, .. } if *named == agent));
        if found || Instant::now() >= deadline {
            break found;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let reply = reply.recv_timeout(Duration::from_secs(3));
    let still_held = check.wait_held(Duration::ZERO);
    check.release();
    assert!(paused, "a local pause is journaled while the check is held");
    let (result, waited) = reply.expect("the reply came while the check was still held");
    assert!(still_held, "the reply came while the check was still held");
    assert!(
        waited < Duration::from_secs(2),
        "the reply waited {waited:?} for a held check"
    );
    assert_eq!(
        result.providers,
        vec![ProviderProbe {
            login_home: None,
            provider: Provider::Claude,
            command: None,
            version: None,
            ok: false,
            detail: Some("the provider check did not finish in time".to_owned()),
            sign_in: None,
        }]
    );
    pause_call
        .join()
        .expect("the pause thread")
        .expect("the pause");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
