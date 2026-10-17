//! the supervisor announces nothing to a provider that assigns its own session id, on a
//! first turn and on a re-issue, and a pinning provider still
//! receives a minted id.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{Entry, ExitStatus, Record, RecordKind, TurnOutcome};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Arrival, Isolation, ManagedInstall, SessionAcquisition, TurnSession,
    UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{LaunchError, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand, RecoveryRefusal};
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, journal_with_issued_turn};
use dudes_supervisor::{Command, Input};
use dudes_types::{Lsn, Mode, ModelSelection, Provenance, Provider, SessionId, Tier, TurnId};

const ASSIGNED: SessionAcquisition = SessionAcquisition::Assigned {
    resume: "--session",
    arrival: Arrival::Start,
};

type Received = Arc<Mutex<Vec<TurnSession>>>;

#[derive(Debug)]
struct Declaring {
    mode: SessionAcquisition,
    received: Received,
}

impl Adapter for Declaring {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::ObservedOnly
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        self.mode
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError> {
        self.received
            .lock()
            .expect("the record is not poisoned")
            .push(session.clone());
        let mut argv = vec!["declaring".into()];
        argv.extend(self.mode.argv(session)?);
        argv.push(task.into());
        Ok(LaunchSpec {
            argv,
            env: Vec::new(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::None
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

fn harness(mode: SessionAcquisition) -> (Harness, Received) {
    let received = Received::default();
    let recorder = Arc::clone(&received);
    let harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        move || {
            Box::new(Declaring {
                mode,
                received: Arc::clone(&recorder),
            })
        },
    );
    (harness, received)
}

fn received(record: &Received) -> Vec<TurnSession> {
    record.lock().expect("the record is not poisoned").clone()
}

fn issued_sessions(harness: &Harness) -> Vec<Option<SessionId>> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::TurnIssued { session, .. } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

fn refused(harness: &Harness) -> usize {
    harness
        .records()
        .filter(|record| {
            matches!(
                record,
                Record::TurnEnded {
                    outcome: Some(TurnOutcome::LaunchRefused { .. }),
                    ..
                }
            )
        })
        .count()
}

#[test]
fn a_first_turn_on_an_assigning_provider_announces_no_session() {
    let (mut assigning, handed) = harness(ASSIGNED);
    assigning.hire_with_goal(&["npm", "test"]);
    assert_eq!(issued_sessions(&assigning), [None]);
    assert_eq!(received(&handed), [TurnSession::Assigned]);
    assert_eq!(refused(&assigning), 0);

    let (mut pinning, handed) = harness(SESSION_ACQUISITION);
    pinning.hire_with_goal(&["npm", "test"]);
    let issued = issued_sessions(&pinning);
    let [Some(minted)] = issued.as_slice() else {
        panic!("control: a pinning provider's first turn announces a minted id, got {issued:?}");
    };
    assert_eq!(received(&handed), [TurnSession::First(minted.clone())]);
}

#[derive(Debug, PartialEq, Eq)]
struct Reissue {
    handed: Vec<TurnSession>,
    spawned: (Option<SessionId>, bool),
    refused: usize,
}

#[derive(Clone, Copy, Debug)]
enum Crash {
    Bare,
    PriorConfirmed,
}

fn insert_prior_confirmed_turn(entries: &mut Vec<Entry>, session: SessionId) {
    let at = entries.last().expect("the journal has entries").at;
    let Record::TurnIssued { agent, goal, .. } = entries[2].record else {
        unreachable!()
    };
    let prior = TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").expect("a distinct turn");
    entries.splice(
        2..2,
        [
            Entry {
                at,
                record: Record::TurnIssued {
                    agent,
                    turn: prior,
                    goal,
                    task: "prior completed work".to_owned(),
                    session: Some(session.clone()),
                    mode: Mode::Managed,
                    model_selection: Some(ModelSelection::provider_default()),
                    lsn_of_decision: Lsn(1),
                    answers: Vec::new(),
                },
            },
            Entry {
                at,
                record: Record::TurnSpawned {
                    agent,
                    turn: prior,
                    pid: 7,
                    start_time: 11,
                    boot: None,
                    session: Some(session.clone()),
                    reissued: false,
                    mcp_grant: None,
                },
            },
            Entry {
                at,
                record: Record::SessionLearned {
                    agent,
                    turn: prior,
                    session: session.clone(),
                },
            },
            Entry {
                at,
                record: Record::TurnEnded {
                    final_reply: None,
                    elapsed_ms: None,
                    session_usage: None,
                    agent,
                    turn: prior,
                    mode: Mode::Managed,
                    provenance: Provenance::Observed,
                    exit: ExitStatus {
                        code: Some(0),
                        signal: None,
                    },
                    usage: None,
                    model_usage: BTreeMap::new(),
                    last_message: Some("continue".to_owned()),
                    session: Some(session),
                    outcome: Some(TurnOutcome::Completed),
                },
            },
        ],
    );
}

fn replay_crash(
    mode: SessionAcquisition,
    session: Option<SessionId>,
    crash: Crash,
    before_restart: impl FnOnce(&mut Harness),
) -> (Harness, Received, TurnId) {
    let (mut entries, agent, turn) =
        journal_with_issued_turn(session.clone(), "make the tests pass");
    if let (Crash::PriorConfirmed, Some(session)) = (crash, session) {
        insert_prior_confirmed_turn(&mut entries, session);
    }
    let (mut harness, handed) = harness(mode);
    harness.journal = entries;
    before_restart(&mut harness);
    harness.restart();
    assert!(received(&handed).is_empty(), "restart waits for consent");
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        received(&handed).is_empty(),
        "ordinary Resume waits for consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .expect("the missing spawn offers an exact repeat")
        .expected;
    assert_eq!(expected.work.turn, turn);
    let request_id = Hex32::new([31; 32]);
    let progress = harness.recover(RecoveryCommand {
        request_id,
        digest: Hex32::new([32; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator {
            pid: 7,
            uid: Some(42),
        },
    });
    assert_eq!(progress.recovery().expect("command progress").refusal, None);
    assert!(harness.records().any(|record| matches!(
        record,
        Record::RecoveryAuthorized(authorization)
            if authorization.request_id == request_id
    )));
    (harness, handed, turn)
}

#[test]
fn a_pre_spawn_historical_confirmation_cannot_authorize_a_repeat() {
    let session = old();
    let (mut entries, agent, turn) =
        journal_with_issued_turn(Some(session.clone()), "make the tests pass");
    let at = entries.last().expect("the journal has entries").at;
    entries.push(Entry {
        at,
        record: Record::SessionLearned {
            agent,
            turn,
            session,
        },
    });
    let (mut harness, handed) = harness(SESSION_ACQUISITION);
    harness.journal = entries;
    harness.restart();
    assert!(harness.supervisor.world().turns[&turn].session_confirmed);
    assert_eq!(
        harness.supervisor.recovery_opportunity(agent),
        Err(RecoveryRefusal::NotRecoverable)
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(received(&handed).is_empty());
    assert_eq!(
        harness.supervisor.recovery_opportunity(agent),
        Err(RecoveryRefusal::NotRecoverable)
    );
}

fn child_exits(harness: &mut Harness, turn: TurnId) {
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::NoResult,
        last_message: None,
    });
}

fn ended_session(harness: &Harness, turn: TurnId) -> Option<SessionId> {
    harness
        .records()
        .find_map(|record| match record {
            Record::TurnEnded {
                turn: ended,
                session,
                ..
            } if *ended == turn => Some(session.clone()),
            _ => None,
        })
        .expect("the re-issued turn ended")
}

fn reissued(mode: SessionAcquisition, session: Option<SessionId>) -> Reissue {
    let (harness, handed, turn) = replay_crash(mode, session, Crash::PriorConfirmed, |_| {});
    Reissue {
        handed: received(&handed),
        spawned: harness.spawned_session(turn),
        refused: refused(&harness),
    }
}

#[test]
fn a_reissue_on_an_assigning_provider_announces_no_session() {
    let old = SessionId::new("11111111-1111-4111-8111-111111111111");
    for issued in [None, Some(old.clone())] {
        assert_eq!(
            reissued(ASSIGNED, issued.clone()),
            Reissue {
                handed: vec![TurnSession::Assigned],
                spawned: (None, true),
                refused: 0,
            },
            "a re-issue from a TurnIssued with session {issued:?}"
        );
    }

    let pinning = reissued(SESSION_ACQUISITION, Some(old.clone()));
    let (Some(minted), true) = pinning.spawned.clone() else {
        panic!("control: a pinning provider's re-issue spawns on a minted id, got {pinning:?}");
    };
    assert_ne!(minted, old, "control: the re-issue announced a fresh id");
    assert_eq!(
        pinning,
        Reissue {
            handed: vec![TurnSession::First(minted.clone())],
            spawned: (Some(minted), true),
            refused: 0,
        }
    );
}

#[derive(Debug, PartialEq, Eq)]
struct Ended {
    spawned: Option<SessionId>,
    ended: Option<SessionId>,
}

fn ended_after_reissue(
    mode: SessionAcquisition,
    crash: Crash,
    learned: Option<SessionId>,
) -> Ended {
    let (mut harness, _handed, turn) = replay_crash(mode, Some(old()), crash, |_| {});
    if let Some(session) = learned {
        harness.drive(Input::SessionLearned { turn, session });
    }
    child_exits(&mut harness, turn);
    Ended {
        spawned: harness.spawned_session(turn).0,
        ended: ended_session(&harness, turn),
    }
}

fn old() -> SessionId {
    SessionId::new("11111111-1111-4111-8111-111111111111")
}

#[test]
fn a_reissued_turn_ends_on_the_session_it_confirmed_or_announced() {
    for crash in [Crash::Bare, Crash::PriorConfirmed] {
        assert_eq!(
            ended_after_reissue(ASSIGNED, crash, None),
            Ended {
                spawned: None,
                ended: None,
            },
            "a {crash:?} re-issue that announced nothing and learned nothing ends on no session"
        );

        let named = SessionId::new("vendor-named-session");
        assert_eq!(
            ended_after_reissue(ASSIGNED, crash, Some(named.clone())),
            Ended {
                spawned: None,
                ended: Some(named),
            },
            "control: a {crash:?} re-issue ends on the session the vendor confirmed"
        );

        let pinning = ended_after_reissue(SESSION_ACQUISITION, crash, None);
        assert!(
            pinning.spawned.is_some() && pinning.spawned != Some(old()),
            "control: a {crash:?} pinning re-issue announced a fresh id, got {pinning:?}"
        );
        assert_eq!(
            pinning.ended, pinning.spawned,
            "control: a {crash:?} pinning re-issue ends on its fresh id"
        );
    }
}

fn ended_before_spawn_folds(
    mode: SessionAcquisition,
    crash: Crash,
    early_end: EarlyEnd,
) -> (Option<SessionId>, Vec<TurnSession>) {
    let (mut harness, handed, turn) =
        replay_crash(mode, Some(old()), crash, |harness| match early_end {
            EarlyEnd::SpawnRefused => {
                harness.host.fail_spawn = Some(std::io::ErrorKind::PermissionDenied);
            }
            EarlyEnd::ExitBeforeAck => {
                harness.unfsynced.insert(RecordKind::TurnSpawned);
            }
        });
    if let EarlyEnd::ExitBeforeAck = early_end {
        child_exits(&mut harness, turn);
    }
    (ended_session(&harness, turn), received(&handed))
}

#[derive(Clone, Copy, Debug)]
enum EarlyEnd {
    SpawnRefused,
    ExitBeforeAck,
}

#[test]
fn a_reissue_that_ends_before_its_spawn_folds_ends_on_what_it_announced() {
    for crash in [Crash::Bare, Crash::PriorConfirmed] {
        for early_end in [EarlyEnd::SpawnRefused, EarlyEnd::ExitBeforeAck] {
            assert_eq!(
                ended_before_spawn_folds(ASSIGNED, crash, early_end),
                (None, vec![TurnSession::Assigned]),
                "a {crash:?} re-issue ended by {early_end:?} ends on no session"
            );

            let (ended, handed) = ended_before_spawn_folds(SESSION_ACQUISITION, crash, early_end);
            let [TurnSession::First(minted)] = handed.as_slice() else {
                panic!("control: a pinning re-issue is handed a minted id, got {handed:?}");
            };
            assert_eq!(
                ended.as_ref(),
                Some(minted),
                "control: a {crash:?} pinning re-issue ended by {early_end:?} ends on its fresh id"
            );
        }
    }
}
