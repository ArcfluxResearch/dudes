//! Rebooted fixtures allocate identities outside their durable prefix.
#![cfg(unix)]

mod common;

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};
use duded::startup::Witness;
use duded::{ExitCause, Settings, boot};
use dudes_proto::Record;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{Method, Response};
use dudes_rpc::Client;
use dudes_supervisor::Clock;
use dudes_types::{GoalId, Timestamp, TurnId};
use serde_json::json;

struct SampleClock {
    inner: TestClock,
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl SampleClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: TestClock::new(),
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        })
    }

    fn hold(self: &Arc<Self>) -> ReleaseSample {
        *self.state.lock().unwrap() = (true, false);
        ReleaseSample(Arc::clone(self))
    }

    fn wait_held(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |s| !s.1)
            .unwrap();
        assert!(state.1, "instruction sampling reaches the explicit barrier");
    }
}

impl Clock for SampleClock {
    fn now(&self) -> Timestamp {
        if matches!(
            std::thread::current().name(),
            Some("dudes-instructions-io" | "dudes-instructions-deadline")
        ) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.0 {
                state.1 = true;
                self.changed.notify_all();
                while state.0 {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
        self.inner.now()
    }
}

struct ReleaseSample(Arc<SampleClock>);

impl Drop for ReleaseSample {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0 = false;
        self.0.changed.notify_all();
    }
}

fn parts(
    settings: &Settings,
    host: SharedHost,
    clock: Arc<SampleClock>,
) -> duded::Seams<SharedHost, ScriptedRunner, SampleClock, common::FixtureRng> {
    let parts = common::seams(
        settings,
        host,
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    duded::Seams {
        host: parts.host,
        pty_host: parts.pty_host,
        predicates: parts.predicates,
        clock,
        rng: parts.rng,
        resolver: parts.resolver,
        adapters: parts.adapters,
        children: parts.children,
        witness: parts.witness,
    }
}

fn list(settings: &Settings) -> Response {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(Method::AgentList, Some(json!({})), credential)
        .unwrap()
}

fn leave_pending_prefix(fixture: &Fixture, settings: &Settings) -> (Vec<TurnId>, Vec<GoalId>) {
    let host = SharedHost::new();
    let clock = SampleClock::new();
    let mut running =
        common::start(boot(settings, parts(settings, host.clone(), Arc::clone(&clock))).unwrap());
    let mut old_turns = Vec::new();
    for _ in 0..2 {
        let agent = common::hire(&running.handle, fixture.state.path());
        let goal = common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/false"]),
        );
        let turn = fixture.await_record("initial issued turn", |record| match record {
            Record::TurnIssued {
                turn, goal: found, ..
            } if *found == goal => Some(*turn),
            _ => None,
        });
        fixture.await_record("initial spawned turn", |record| {
            matches!(record, Record::TurnSpawned { turn: seen, .. } if *seen == turn).then_some(())
        });
        running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
        common::end_turn(&running.handle, turn);
        fixture.await_record("initial turn ended", |r| {
            matches!(r, Record::TurnEnded { turn: seen, .. } if *seen == turn).then_some(())
        });
        fixture.await_record("initial turn instruction settlement", |r| matches!(r, Record::InstructionObservationFinished { owner: dudes_proto::InstructionObservationOwner::Managed { turn: seen, .. }, .. } if *seen == turn).then_some(()));
        old_turns.push(turn);
    }
    let release = clock.hold();
    let mut pending = Vec::new();
    for _ in 0..2 {
        let agent = common::hire(&running.handle, fixture.state.path());
        let goal = common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/false"]),
        );
        fixture.await_record("pending goal decision", |r| {
            matches!(r, Record::LoopDecided { agent: seen, .. } if *seen == agent).then_some(())
        });
        clock.wait_held();
        pending.push(goal);
    }
    clock.wait_held();
    let prefix = fixture.entries();
    assert!(pending.iter().all(|goal| !prefix.iter().any(|(_, entry)| matches!(&entry.record, Record::TurnIssued { goal: found, .. } if found == goal))));
    drop(release);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    std::fs::remove_dir_all(fixture.journal_dir()).unwrap();
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    for (_, entry) in prefix {
        writer.append(&entry).unwrap();
    }
    drop(writer);
    let before = fixture.records();
    assert!(pending.iter().all(|goal| {
        !before
            .iter()
            .any(|r| matches!(r, Record::TurnIssued { goal: found, .. } if found == goal))
    }));

    (old_turns, pending)
}

#[test]
fn pending_instruction_samples_issue_fresh_turns_after_reboot() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let (old_turns, pending) = leave_pending_prefix(&fixture, &settings);
    let clock = SampleClock::new();
    let release = clock.hold();
    let daemon = boot(
        &settings,
        parts(&settings, SharedHost::new(), Arc::clone(&clock)),
    )
    .unwrap();
    let mut restarted = common::start(daemon);
    clock.wait_held();
    assert!(list(&settings).error.is_none());
    drop(release);
    for goal in pending {
        let until = Instant::now() + Duration::from_secs(5);
        let turn = loop {
            if let Some(turn) = fixture.records().iter().find_map(|r| match r {
                Record::TurnIssued {
                    turn, goal: found, ..
                } if *found == goal => Some(*turn),
                _ => None,
            }) {
                break turn;
            }
            if restarted.handle.doctor().is_err() {
                restarted.join();
                panic!("the rebooted daemon exited before issuing the pending goal");
            }
            assert!(
                Instant::now() < until,
                "the pending goal receives a post-reboot turn"
            );
            std::thread::yield_now();
        };
        assert!(
            !old_turns.contains(&turn),
            "reboot cannot reuse a recorded TurnId"
        );
    }
    assert!(list(&settings).error.is_none());
    assert_eq!(
        restarted.stop(ExitCause::ShutdownRpc),
        ExitCause::ShutdownRpc
    );
}
