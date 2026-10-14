//! Local replacement admission and recovery over the production socket.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::record::Hex32;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::goal::{CompletionCheckDeclaration, GoalReplaceParams, GoalReplaceResult};
use dudes_proto::rpc::{ErrorCode, Method, Request, RequestId, Response};
use dudes_proto::{Predicate, Record};
use dudes_rpc::Client;
use dudes_types::{AgentId, GoalId, TurnId};
use serde_json::{Value, json};

mod common;
use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

fn credential(settings: &duded::Settings) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap()
}

fn call(settings: &duded::Settings, method: Method, params: Value) -> Response {
    Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(method, Some(params), credential(settings))
        .unwrap()
}

fn start(fixture: &Fixture) -> (duded::Settings, common::Running) {
    let settings = fixture.settings();
    let daemon = boot(&settings, replay_seams(fixture)).unwrap();
    (settings, common::start(daemon))
}

fn replay_seams(
    fixture: &Fixture,
) -> duded::settings::Seams<SharedHost, ScriptedRunner, TestClock, dudes_supervisor::testing::FakeRng>
{
    let mut seams = common::seams(
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    for _ in 0..fixture.records().len() * 8 {
        dudes_supervisor::Rng::ulid(&mut seams.rng);
    }
    seams
}

fn native_goal_seams<P: duded::predicate::PredicateRunner>(
    fixture: &Fixture,
    runner: Arc<P>,
) -> (
    duded::settings::Seams<SharedHost, P, TestClock, dudes_supervisor::testing::FakeRng>,
    SharedHost,
) {
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let parts = common::seams(
        host.clone(),
        runner,
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&parts.children));
    (parts, host)
}

fn issued_goal(fixture: &Fixture, goal: GoalId) -> TurnId {
    fixture.await_record("the exact goal's issue", |record| match record {
        Record::TurnIssued {
            goal: issued, turn, ..
        } if *issued == goal => Some(*turn),
        _ => None,
    })
}

fn spawned_goal(fixture: &Fixture, goal: GoalId) -> TurnId {
    let turn = issued_goal(fixture, goal);
    fixture.await_record("the exact goal's native spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: seen, .. } if *seen == turn).then_some(())
    });
    turn
}

fn paused_goal(
    fixture: &Fixture,
    settings: &duded::Settings,
    running: &common::Running,
) -> (AgentId, GoalId) {
    let agent = common::hire(&running.handle, fixture.state.path());
    assert_eq!(
        call(settings, Method::LoopPause, json!({"agent":agent})).result,
        Some(json!({}))
    );
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    (agent, goal)
}

fn request(agent: AgentId, expected_goal: GoalId, id: u8) -> GoalReplaceParams {
    GoalReplaceParams {
        request_id: Hex32::new([id; 32]),
        agent,
        expected_goal,
        text: "reviewed replacement".into(),
        predicate: Predicate::Command {
            argv: vec!["/usr/bin/true".into(), "literal; $(not-a-shell)".into()],
            timeout_s: 23,
        },
        check: CompletionCheckDeclaration {
            id: "reviewed-case".into(),
            label: "Reviewed case".into(),
        },
        may_already_pass: true,
    }
}

fn replace(settings: &duded::Settings, request: &GoalReplaceParams) -> GoalReplaceResult {
    let response = call(
        settings,
        Method::GoalReplace,
        serde_json::to_value(request).unwrap(),
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    serde_json::from_value(response.result.unwrap()).unwrap()
}

struct ReleaseWriter(duded::runtime::OnboardingPause);
impl Drop for ReleaseWriter {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool, description: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "{description}");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn queued_replacement(
    answer: &std::sync::mpsc::Receiver<Result<duded::ports::Answered, dudes_proto::rpc::RpcError>>,
) -> GoalReplaceResult {
    let duded::ports::Answered::GoalReplaced(result) = answer
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap()
    else {
        panic!("replacement reply expected")
    };
    result
}

#[test]
fn replacement_is_atomic_preserves_pause_and_retries_after_later_goals_and_restart() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 1);
    let changed = replace(&settings, &req);
    let GoalReplaceResult::Changed {
        goal,
        completion_check,
        ..
    } = &changed
    else {
        panic!("replacement was not durable")
    };
    let record = fixture
        .records()
        .into_iter()
        .find(|r| matches!(r,Record::GoalSet {goal:new,..} if new.id==*goal))
        .unwrap();
    let Record::GoalSet {
        goal: new,
        goal_replacement,
        browser_goal,
        browser_onboarding,
        ..
    } = record
    else {
        unreachable!()
    };
    assert_eq!(new.text, req.text);
    assert_eq!(new.predicate, req.predicate);
    assert_eq!(new.completion_check.as_ref(), Some(completion_check));
    assert_eq!(new.hop_cap, 8);
    assert_eq!(new.budget.usd, Some(dudes_types::Usd::from_dollars(5)));
    assert!(browser_goal.is_none() && browser_onboarding.is_none());
    assert_eq!(goal_replacement.unwrap().replaced_goal, old);
    let rows = call(&settings, Method::AgentList, json!({}))
        .result
        .unwrap();
    assert_eq!(rows[0]["paused"], true);
    assert_eq!(
        rows[0]["goal"]["completion_check"],
        serde_json::to_value(completion_check).unwrap()
    );
    assert!(rows[0]["goal"].get("last_result").is_none());
    assert!(!rows.to_string().contains("not-a-shell"));
    let later = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    assert_ne!(later, *goal);
    assert_eq!(replace(&settings, &req), changed);
    let mut conflict = req.clone();
    conflict.check.label = "Changed".into();
    assert!(matches!(
        replace(&settings, &conflict),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::RequestConflict,
            ..
        }
    ));
    assert!(matches!(
        replace(&settings, &request(agent, old, 2)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::StaleGoal,
            ..
        }
    ));
    running.stop(ExitCause::Ended);
    let (_, mut restarted) = start(&fixture);
    assert_eq!(replace(&settings, &req), changed);
    restarted.stop(ExitCause::Ended);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(
                r,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn raw_ingress_rejects_duplicates_and_exact_overflow_before_effects() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let params = request(agent, old, 2);
    let envelope = Request {
        jsonrpc: "2.0".into(),
        id: RequestId::Number(1),
        method: "goal.replace".into(),
        params: Some(serde_json::to_value(params).unwrap()),
        v: Some(1),
        cred: Some(credential(&settings)),
    };
    let original = serde_json::to_string(&envelope).unwrap();
    for (line, code) in [
        (
            original.replace("\"v\":1", "\"v\":2"),
            ErrorCode::InvalidRequest,
        ),
        (
            original.replacen('{', "{\"extra\":0,", 1),
            ErrorCode::InvalidRequest,
        ),
        (
            original.replacen('{', "{\"i\\u0064\":2,", 1),
            ErrorCode::InvalidRequest,
        ),
        (
            original.replace("\"text\":", "\"te\\u0078t\":\"other\",\"text\":"),
            ErrorCode::InvalidParams,
        ),
        (
            original.replace("\"argv\":", "\"ar\\u0067v\":[],\"argv\":"),
            ErrorCode::InvalidParams,
        ),
        (
            original.replace("\"check\":{", "\"check\":{\"extra\":0,"),
            ErrorCode::InvalidParams,
        ),
    ] {
        let before = fixture.records();
        let response = Client::connect(settings.socket())
            .unwrap()
            .send_raw(&line)
            .unwrap();
        assert_eq!(response.error.unwrap().code, code.code());
        assert_eq!(fixture.records(), before);
    }
    let mut line = original;
    line.extend(std::iter::repeat_n(' ', 131_072 - line.len()));
    let overflow = format!("{line} ");
    assert_eq!(
        Client::connect(settings.socket())
            .unwrap()
            .send_raw(&overflow)
            .unwrap()
            .error
            .unwrap()
            .code,
        ErrorCode::InvalidParams.code()
    );
    let changed = Client::connect(settings.socket())
        .unwrap()
        .send_raw(&line)
        .unwrap()
        .result
        .unwrap();
    assert_eq!(changed["state"], "changed");
    assert_eq!(
        Client::connect(settings.socket())
            .unwrap()
            .send_raw(&format!("{line}\r"))
            .unwrap()
            .result,
        Some(changed)
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn refusal_order_preserves_other_agents_and_open_turns() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let other = common::hire(&running.handle, fixture.state.path());
    let other_goal = common::set_goal(
        &running.handle,
        common::goal_params(other, &["/usr/bin/false"]),
    );
    spawned_goal(&fixture, other_goal);
    running.handle.doctor().unwrap();
    let absent = replace(&settings, &request(AgentId::NIL, old, 1));
    assert!(matches!(
        absent,
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::AgentAbsent,
            ..
        }
    ));
    let busy = replace(&settings, &request(other, other_goal, 2));
    assert!(matches!(
        busy,
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
            ..
        }
    ));
    assert!(matches!(
        replace(&settings, &request(agent, old, 3)),
        GoalReplaceResult::Changed { .. }
    ));
    call(&settings, Method::LoopHalt, json!({"agent":agent}));
    let halted = replace(&settings, &request(agent, old, 4));
    assert!(matches!(
        halted,
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::AgentHalted,
            ..
        }
    ));
    let rows = call(&settings, Method::AgentList, json!({}))
        .result
        .unwrap();
    let other_row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == other.to_string())
        .unwrap();
    assert_eq!(other_row["paused"], false);
    assert_eq!(other_row["goal"]["id"], other_goal.to_string());
    running.stop(ExitCause::Ended);
}

#[test]
fn invalid_record_cannot_bypass_startup_validation_through_cached_projections() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    replace(&settings, &request(agent, old, 7));
    running.stop(ExitCause::Ended);
    let mut invalid = fixture
        .entries()
        .into_iter()
        .find_map(|(_, entry)| {
            matches!(
                entry.record,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            )
            .then_some(entry)
        })
        .unwrap();
    if let Record::GoalSet {
        goal,
        goal_replacement,
        ..
    } = &mut invalid.record
    {
        goal.id = GoalId::from_parts(90, 90);
        goal.completion_check.as_mut().unwrap().label = "Forged label".into();
        *goal_replacement = None;
    }
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    writer.append(&invalid).unwrap();
    drop(writer);
    let mut projections =
        dudes_journal::Projections::open(&settings.state_dir.join("state.redb")).unwrap();
    dudes_journal::replay(
        &dudes_journal::Reader::new(&fixture.journal_dir()),
        &mut projections,
    )
    .unwrap();
    drop(projections);
    let result = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    );
    let error = match result {
        Ok(_) => panic!("malformed history admitted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("GoalSet check revision does not match its content"),
        "{error}"
    );
    assert!(std::os::unix::net::UnixStream::connect(settings.socket()).is_err());
}

#[test]
fn reservation_reports_pending_and_conflict_without_duplicating_the_append() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let pause = daemon.onboarding_pause();
    let mut running = common::start(daemon);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 8);
    pause.arm(duded::runtime::OnboardingPausePoint::BeforeGoalChangePermit);
    let _release = ReleaseWriter(pause.clone());
    let first = {
        let settings = settings.clone();
        let req = req.clone();
        std::thread::spawn(move || replace(&settings, &req))
    };
    assert!(pause.wait_reached(std::time::Duration::from_secs(5)));
    assert_eq!(
        replace(&settings, &req),
        GoalReplaceResult::Pending {
            request_id: req.request_id
        }
    );
    let mut conflict = req.clone();
    conflict.text = "different".into();
    assert!(matches!(
        replace(&settings, &conflict),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::RequestConflict,
            ..
        }
    ));
    pause.release();
    let changed = first.join().unwrap();
    assert!(matches!(changed, GoalReplaceResult::Changed { .. }));
    assert_eq!(replace(&settings, &req), changed);
    running.stop(ExitCause::Ended);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(
                r,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn own_uncertain_fsync_stays_pending_and_replays_exactly_once() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.onboarding_failure();
    let mut running = common::start(daemon);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 9);
    failure.arm(duded::runtime::OnboardingFailurePoint::LocalGoal);
    assert_eq!(
        replace(&settings, &req),
        GoalReplaceResult::Pending {
            request_id: req.request_id
        }
    );
    assert_eq!(
        replace(&settings, &req),
        GoalReplaceResult::Pending {
            request_id: req.request_id
        }
    );
    assert!(matches!(
        replace(&settings, &request(agent, old, 10)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::Unavailable,
            ..
        }
    ));
    running.stop(ExitCause::Ended);
    let (_, mut restarted) = start(&fixture);
    assert!(matches!(
        replace(&settings, &req),
        GoalReplaceResult::Changed { .. }
    ));
    restarted.stop(ExitCause::Ended);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(
                r,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[derive(Clone, Copy, Default)]
enum CheckEnd {
    #[default]
    Verdict,
    NoVerdict,
    Swept,
    Panic,
}

#[derive(Default)]
struct HeldCheck {
    end: CheckEnd,
    released: (std::sync::Mutex<bool>, std::sync::Condvar),
    started: (std::sync::Mutex<bool>, std::sync::Condvar),
}
impl HeldCheck {
    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }
    fn await_started(&self) {
        let ready = self.started.0.lock().unwrap();
        let (ready, _) = self
            .started
            .1
            .wait_timeout_while(ready, std::time::Duration::from_secs(5), |ready| !*ready)
            .unwrap();
        assert!(*ready);
    }
}
impl duded::predicate::PredicateRunner for HeldCheck {
    fn run(
        &self,
        _spec: &dudes_providers::launch::LaunchSpec,
        _timeout: std::time::Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        *self.started.0.lock().unwrap() = true;
        self.started.1.notify_all();
        let ready = self.released.0.lock().unwrap();
        drop(self.released.1.wait_while(ready, |ready| !*ready).unwrap());
        match self.end {
            CheckEnd::Verdict => Ok(common::passes()),
            CheckEnd::NoVerdict => Err(std::io::Error::other("owned missing verdict")),
            CheckEnd::Swept => Ok(duded::predicate::PredicateOutcome::Swept),
            CheckEnd::Panic => panic!("owned predicate unwind"),
        }
    }
    fn sweep(&self) {
        self.release();
    }
}

#[test]
fn older_goal_check_blocks_only_its_owner_until_writer_settlement() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let runner = Arc::new(HeldCheck::default());
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let old = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = fixture.await_turn();
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    common::end_turn(&running.handle, turn);
    runner.await_started();
    let current = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    assert_ne!(current, old);
    assert!(matches!(
        replace(&settings, &request(agent, current, 1)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
            ..
        }
    ));
    let (other, other_goal) = paused_goal(&fixture, &settings, &running);
    assert!(matches!(
        replace(&settings, &request(other, other_goal, 2)),
        GoalReplaceResult::Changed { .. }
    ));
    runner.release();
    fixture.await_record("old check settled", |record| {
        matches!(record,Record::PredicateResult {goal,..} if *goal==old).then_some(())
    });
    assert!(matches!(
        replace(&settings, &request(agent, current, 1)),
        GoalReplaceResult::Changed { .. }
    ));
    running.stop(ExitCause::Ended);
}

#[test]
fn known_no_effect_append_failure_can_retry_without_spending_or_replenishing() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 11);
    let before = fixture.records();
    failure.fail_nth(std::num::NonZeroUsize::new(1).unwrap());
    assert!(matches!(
        replace(&settings, &req),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::Unavailable,
            ..
        }
    ));
    assert_eq!(fixture.records(), before);
    assert!(matches!(
        replace(&settings, &req),
        GoalReplaceResult::Changed { .. }
    ));
    running.stop(ExitCause::Ended);
}

#[test]
fn post_fold_continuation_failure_replays_one_atomic_replacement() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let seams = replay_seams(&fixture);
    let host = seams.host.clone();
    let daemon = boot(&settings, seams).unwrap();
    let failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    let mut source = common::goal_params(agent, &["/usr/bin/false"]);
    source.budget = dudes_proto::GoalBudget {
        usd: Some(dudes_types::Usd::from_dollars(3)),
        tokens: Some(dudes_types::Tokens::new(77)),
    };
    source.hop_cap = 4;
    let budget = source.budget;
    let old = common::set_goal(&running.handle, source);
    let req = request(agent, old, 70);
    let before = fixture.records().len();
    failure.fail_nth(std::num::NonZeroUsize::new(2).unwrap());
    let changed = replace(&settings, &req);
    let GoalReplaceResult::Changed {
        goal: id,
        completion_check,
        ..
    } = &changed
    else {
        panic!("the durable replacement result must survive its interrupted continuation")
    };
    let records = fixture.records();
    let [stored] = &records[before..] else {
        panic!("the interrupted pump must append only its replacement GoalSet")
    };
    let Record::GoalSet {
        agent: owner,
        goal,
        goal_replacement: Some(meta),
        browser_goal: None,
        browser_onboarding: None,
    } = stored
    else {
        panic!("one operator replacement expected")
    };
    assert_eq!(*owner, agent);
    assert_eq!(goal.id, *id);
    assert_ne!(goal.id, old);
    assert_eq!(goal.text, req.text);
    assert_eq!(goal.predicate, req.predicate);
    assert_eq!(goal.completion_check.as_ref(), Some(completion_check));
    assert_eq!(goal.budget, budget);
    assert_eq!(goal.hop_cap, 4);
    assert_eq!(meta.replaced_goal, old);
    assert_eq!(meta.reserved, dudes_proto::Spend::default());
    assert!(host.spawned().is_empty());
    assert!(matches!(
        replace(&settings, &request(agent, old, 71)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::StaleGoal,
            ..
        }
    ));
    assert_eq!(fixture.records(), records);
    let probe_start = records.len();
    assert!(
        call(&settings, Method::LoopPause, json!({"agent":agent}))
            .error
            .is_none(),
        "the second-append fault must be consumed before this explicit paused-state probe"
    );
    let probed = fixture.records();
    assert!(matches!(&probed[probe_start..], [Record::Pause { agent: who, .. }] if *who == agent));
    assert_eq!(replace(&settings, &req), changed);
    assert_eq!(fixture.records(), probed);
    running.stop(ExitCause::Ended);
    let (_, mut restarted) = start(&fixture);
    let replayed = fixture.records();
    assert_eq!(replace(&settings, &req), changed);
    assert_eq!(fixture.records(), replayed);
    let replacements: Vec<_> = replayed
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(replacements, [stored]);
    assert!(!replayed.iter().any(|record| matches!(
        record,
        Record::GoalCleared { .. } | Record::TurnIssued { .. } | Record::TurnSpawned { .. }
    )));
    restarted.stop(ExitCause::Ended);
}

#[test]
fn ordinary_operator_admission_refuses_a_recorded_agent_with_copied_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let column = common::ScriptedColumn::new(dudes_rpc::auth::PeerProvenance::NoRecordedAgent);
    let daemon = boot(
        &settings,
        common::seams_with(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            column.clone(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 12);
    column.set(dudes_rpc::auth::PeerProvenance::Agent(agent));
    let before = fixture.records();
    assert_eq!(
        call(
            &settings,
            Method::GoalReplace,
            serde_json::to_value(&req).unwrap()
        )
        .error
        .unwrap()
        .code,
        ErrorCode::Unauthorized.code()
    );
    assert_eq!(fixture.records(), before);
    column.set(dudes_rpc::auth::PeerProvenance::Unresolvable);
    assert!(matches!(
        replace(&settings, &req),
        GoalReplaceResult::Changed { .. }
    ));
    running.stop(ExitCause::Ended);
}

#[test]
fn explicit_operator_command_replaces_a_different_existing_predicate() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, _) = paused_goal(&fixture, &settings, &running);
    let mut source = common::goal_params(agent, &["/usr/bin/false"]);
    source.predicate = Predicate::BoardState {
        card: dudes_types::CardId::NIL,
        status: dudes_types::CardStatus::Done,
    };
    let old = common::set_goal(&running.handle, source);
    let req = request(agent, old, 13);
    let GoalReplaceResult::Changed { goal, .. } = replace(&settings, &req) else {
        panic!("operator replacement refused")
    };
    let current = fixture
        .records()
        .into_iter()
        .find_map(|r| match r {
            Record::GoalSet { goal: current, .. } if current.id == goal => Some(current),
            _ => None,
        })
        .unwrap();
    assert_eq!(current.predicate, req.predicate);
    running.stop(ExitCause::Ended);
}

#[test]
fn optional_budget_units_survive_repeated_replacements_and_zero_caps_refuse() {
    use dudes_proto::rpc::goal::GoalReplaceRefusal;
    use dudes_types::{Tokens, Usd};
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, _) = paused_goal(&fixture, &settings, &running);
    for (index, (usd, tokens)) in [
        (None, None),
        (Some(Usd::from_dollars(5)), None),
        (None, Some(Tokens::new(100))),
        (Some(Usd::from_dollars(5)), Some(Tokens::new(100))),
    ]
    .into_iter()
    .enumerate()
    {
        let mut source = common::goal_params(agent, &["/usr/bin/true"]);
        source.budget = dudes_proto::GoalBudget { usd, tokens };
        let mut current = common::set_goal(&running.handle, source);
        for repeat in 0..2 {
            let identity = u8::try_from(20 + index * 2 + repeat).unwrap();
            let GoalReplaceResult::Changed { goal, .. } =
                replace(&settings, &request(agent, current, identity))
            else {
                panic!("optional allowance refused")
            };
            current = goal;
            let copied = fixture
                .records()
                .into_iter()
                .find_map(|r| match r {
                    Record::GoalSet { goal: row, .. } if row.id == goal => Some(row),
                    _ => None,
                })
                .unwrap();
            assert_eq!(copied.budget, dudes_proto::GoalBudget { usd, tokens });
            assert_eq!(copied.hop_cap, 8);
        }
    }
    for (index, (usd, tokens, hop_cap)) in [
        (Some(Usd::ZERO), None, 8),
        (None, Some(Tokens::ZERO), 8),
        (None, None, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let mut source = common::goal_params(agent, &["/usr/bin/true"]);
        source.budget = dudes_proto::GoalBudget { usd, tokens };
        source.hop_cap = hop_cap;
        let current = common::set_goal(&running.handle, source);
        let before = fixture.records();
        assert!(matches!(
            replace(
                &settings,
                &request(agent, current, u8::try_from(30 + index).unwrap())
            ),
            GoalReplaceResult::Refused {
                reason: GoalReplaceRefusal::GoalExhausted,
                ..
            }
        ));
        assert_eq!(fixture.records(), before);
    }
    running.stop(ExitCause::Ended);
}

#[test]
fn uncertain_replacement_fences_ordinary_commands_and_internal_work_until_replay() {
    for point in [
        duded::runtime::OnboardingFailurePoint::LocalGoal,
        duded::runtime::OnboardingFailurePoint::SpendRefresh,
    ] {
        let fixture = Fixture::new();
        let settings = fixture.settings();
        let host = SharedHost::new();
        let daemon = boot(
            &settings,
            common::seams(
                host.clone(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .unwrap();
        let failure = daemon.onboarding_failure();
        let mut running = common::start(daemon);
        let (agent, old) = paused_goal(&fixture, &settings, &running);
        let req = request(agent, old, 40);
        let memory = || duded::ports::Ask::MemoryAppend {
            agent,
            payload: b"before the fence".to_vec(),
            provenance: dudes_proto::PeerProvenance::Unresolvable,
        };
        assert!(running.handle.ask(memory()).is_ok());
        failure.arm(point);
        let attempted = replace(&settings, &req);
        if point == duded::runtime::OnboardingFailurePoint::LocalGoal {
            assert_eq!(
                attempted,
                GoalReplaceResult::Pending {
                    request_id: req.request_id
                }
            );
        } else {
            assert!(matches!(attempted, GoalReplaceResult::Changed { .. }));
        }
        let durable = fixture.records();
        let spawns = host.spawned().len();
        let set = call(
            &settings,
            Method::GoalSet,
            serde_json::to_value(common::goal_params(agent, &["/usr/bin/false"])).unwrap(),
        );
        assert!(
            set.error.is_some(),
            "ordinary goal.set must not append behind an uncertain replacement: {point:?}"
        );
        assert!(
            call(&settings, Method::LoopResume, json!({"agent":agent}))
                .error
                .is_some()
        );
        assert!(running.handle.ask(memory()).is_err());
        running.handle.input(dudes_supervisor::Input::Tick);
        running.handle.input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Resume {
                agent,
                browser: None,
            },
        ));
        assert!(
            call(&settings, Method::AgentList, json!({}))
                .error
                .is_none()
        );
        assert_eq!(fixture.records(), durable);
        assert_eq!(host.spawned().len(), spawns);
        running.stop(ExitCause::Ended);
        let (_, mut restarted) = start(&fixture);
        let replayed = replace(&settings, &req);
        assert!(matches!(replayed, GoalReplaceResult::Changed { .. }));
        if point == duded::runtime::OnboardingFailurePoint::SpendRefresh {
            assert_eq!(replayed, attempted);
        }
        assert!(
            call(&settings, Method::LoopResume, json!({"agent":agent}))
                .error
                .is_none()
        );
        fixture.await_record("replacement turn after replay", |record| {
            matches!(record,Record::TurnIssued {agent:who,..} if *who==agent).then_some(())
        });
        restarted.stop(ExitCause::Ended);
    }
}

#[test]
fn uncertain_journal_never_allows_a_hook_or_delivers_new_context() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let column = common::ScriptedColumn::new(dudes_rpc::auth::PeerProvenance::NoRecordedAgent);
    let daemon = boot(
        &settings,
        common::seams_with(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            column.clone(),
        ),
    )
    .unwrap();
    let failure = daemon.onboarding_failure();
    let mut running = common::start(daemon);
    let (agent, goal) = paused_goal(&fixture, &settings, &running);
    let worker = common::hire(&running.handle, fixture.state.path());
    let worker_goal = common::set_goal(
        &running.handle,
        common::goal_params(worker, &["/usr/bin/false"]),
    );
    spawned_goal(&fixture, worker_goal);
    running.handle.doctor().unwrap();
    let hash = fixture.await_record("worker token", |record| match record {
        Record::HookTokenMinted { agent, token_hash } if *agent == worker => {
            Some(token_hash.clone())
        }
        _ => None,
    });
    let token = common::token_of(&hash);
    let hook = || {
        column.set(dudes_rpc::auth::PeerProvenance::Agent(worker));
        let response = Client::connect(settings.socket()).unwrap().call(
            Method::HookEvent,
            Some(json!({"token":token.expose(),"raw":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"true"}}})),
        ).unwrap();
        column.set(dudes_rpc::auth::PeerProvenance::NoRecordedAgent);
        assert!(response.error.is_none(), "{:?}", response.error);
        response
    };
    assert_eq!(hook().result.unwrap()["exit_code"], 0);
    failure.arm(duded::runtime::OnboardingFailurePoint::LocalGoal);
    assert!(matches!(
        replace(&settings, &request(agent, goal, 41)),
        GoalReplaceResult::Pending { .. }
    ));
    let frozen = fixture.records();
    let result = hook().result.unwrap();
    assert_eq!(result["exit_code"], 2);
    assert_eq!(
        result["body"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert!(
        result["body"]["hookSpecificOutput"]
            .get("additionalContext")
            .is_none()
    );
    assert_eq!(fixture.records(), frozen);
    running.stop(ExitCause::Ended);
}

#[test]
fn uncertain_replacement_preserves_mailbox_sources_without_new_journal_or_delivery() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.onboarding_failure();
    let mut running = common::start(daemon);
    let (agent, goal) = paused_goal(&fixture, &settings, &running);
    let recipient = common::hire(&running.handle, fixture.state.path());
    let outbox = fixture
        .state
        .path()
        .join("agents")
        .join(agent.to_string())
        .join("outbox");
    std::fs::create_dir_all(&outbox).unwrap();
    let send = |id: dudes_types::MailId, body: &str| {
        let path = outbox.join(format!("{id}.json"));
        std::fs::write(&path,serde_json::to_vec(&json!({"to":{"kind":"agent","agent":recipient},"act":"message","subject":"owned coherence check","body":body})).unwrap()).unwrap();
        path
    };
    let before = dudes_types::MailId::from_parts(90, 1);
    send(before, "before");
    let delivered = dudes_journal::mail_recipient_id(before, recipient);
    fixture.await_record("healthy mailbox control", |record| {
        matches!(record,Record::Mail {id,..} if *id==delivered).then_some(())
    });
    failure.arm(duded::runtime::OnboardingFailurePoint::LocalGoal);
    assert!(matches!(
        replace(&settings, &request(agent, goal, 42)),
        GoalReplaceResult::Pending { .. }
    ));
    let frozen = fixture.records();
    let after = dudes_types::MailId::from_parts(90, 2);
    let source = send(after, &"body".repeat(1500));
    std::thread::sleep(std::time::Duration::from_millis(300));
    running.handle.doctor().unwrap();
    assert_eq!(fixture.records(), frozen);
    assert!(source.exists());
    let inbox = fixture
        .state
        .path()
        .join("agents")
        .join(recipient.to_string())
        .join("inbox");
    let undelivered = dudes_journal::mail_recipient_id(after, recipient);
    assert!(!inbox.join(format!("{undelivered}.json")).exists());
    assert!(
        !inbox
            .join(".staging")
            .join(format!("{undelivered}.body.tmp"))
            .exists()
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn a_new_billing_mode_applies_existing_conditional_cap_rules_without_changing_durable_retries() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor_usd_per_day = None;
    let boot_owned = |settings: &duded::Settings| boot(settings, replay_seams(&fixture)).unwrap();
    let mut running = common::start(boot_owned(&settings));
    let mut hire = common::hire_params(fixture.state.path());
    hire.budget = dudes_proto::AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    };
    let agent: AgentId = serde_json::from_value::<dudes_proto::rpc::agent::HireResult>(
        call(
            &settings,
            Method::AgentHire,
            serde_json::to_value(hire).unwrap(),
        )
        .result
        .unwrap(),
    )
    .unwrap()
    .agent;
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    let mut source = common::goal_params(agent, &["/usr/bin/false"]);
    source.budget = dudes_proto::GoalBudget {
        usd: None,
        tokens: None,
    };
    let old = common::set_goal(&running.handle, source);
    let original = request(agent, old, 43);
    let durable = replace(&settings, &original);
    let GoalReplaceResult::Changed { goal, .. } = durable else {
        panic!("uncapped no-key control refused")
    };
    running.stop(ExitCause::Ended);
    settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "ANTHROPIC_API_KEY",
            "test-only-not-a-credential",
        ));
    let mut restarted = common::start(boot_owned(&settings));
    assert_eq!(replace(&settings, &original), durable);
    let before = fixture.records();
    let refused = call(
        &settings,
        Method::GoalReplace,
        serde_json::to_value(request(agent, goal, 44)).unwrap(),
    )
    .error
    .unwrap();
    assert_eq!(refused.code, ErrorCode::Refused.code());
    assert_eq!(refused.data.unwrap()["gate"], "ceiling_required");
    assert_eq!(fixture.records(), before);
    let funded = common::set_goal(
        &restarted.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    assert!(matches!(
        replace(&settings, &request(agent, funded, 45)),
        GoalReplaceResult::Changed { .. }
    ));
    restarted.stop(ExitCause::Ended);
}

#[test]
fn a_coherence_fence_keeps_operator_memory_diagnostics_but_refuses_agent_reads() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.onboarding_failure();
    let mut running = common::start(daemon);
    let (agent, _) = paused_goal(&fixture, &settings, &running);
    running
        .handle
        .ask(duded::ports::Ask::MemoryAppend {
            agent,
            payload: b"own memory".to_vec(),
            provenance: dudes_proto::PeerProvenance::Unresolvable,
        })
        .unwrap();
    let read = |caller| duded::ports::Ask::MemoryRead {
        agent,
        caller,
        since_lsn: None,
    };
    assert!(matches!(
        running.handle.ask(read(Some(agent))),
        Ok(duded::ports::Answered::MemoryRead(_))
    ));
    assert!(matches!(
        running.handle.ask(duded::ports::Ask::TeamRoster {
            caller: Some(agent)
        }),
        Ok(duded::ports::Answered::Roster(_))
    ));
    failure.arm(duded::runtime::OnboardingFailurePoint::LocalRetire);
    assert!(
        call(
            &settings,
            Method::AgentRetire,
            json!({"agent":agent,"reason":"owned fault control"})
        )
        .error
        .is_some()
    );
    let frozen = fixture.records();
    assert!(running.handle.ask(read(Some(agent))).is_err());
    assert!(
        running
            .handle
            .ask(duded::ports::Ask::TeamRoster {
                caller: Some(agent)
            })
            .is_err()
    );
    assert!(matches!(
        running
            .handle
            .ask(duded::ports::Ask::TeamRoster { caller: None }),
        Ok(duded::ports::Answered::Roster(_))
    ));
    let duded::ports::Answered::MemoryRead(result) = running.handle.ask(read(None)).unwrap() else {
        panic!("operator diagnostic refused")
    };
    assert!(result.text.contains("own memory"));
    assert_eq!(fixture.records(), frozen);
    running.stop(ExitCause::Ended);
}

#[test]
fn replacement_during_closing_preserves_the_frozen_participants_and_issue_fence() {
    use dudes_proto::rpc::closing::{ClosingParams, ClosingStarted, ClosingStatus};
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let (parts, host) = native_goal_seams(
        &fixture,
        Arc::new(ScriptedRunner::scripted([common::passes()])),
    );
    let daemon = boot(&settings, parts).unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let old = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = spawned_goal(&fixture, old);
    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    fixture.await_record("completed nonparticipant",|record|matches!(record,Record::LoopDecided {agent:who,decision:dudes_proto::LoopDecision::Done,..} if *who==agent).then_some(()));
    let participant = common::hire(&running.handle, fixture.state.path());
    let participant_goal = common::set_goal(
        &running.handle,
        common::goal_params(participant, &["/usr/bin/true"]),
    );
    spawned_goal(&fixture, participant_goal);
    running.handle.doctor().unwrap();
    let started: ClosingStarted = serde_json::from_value(
        call(
            &settings,
            Method::SystemClosingTime,
            serde_json::to_value(ClosingParams::Announce).unwrap(),
        )
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(started.participants, [participant]);
    let before = fixture.records();
    let GoalReplaceResult::Changed { goal, .. } = replace(&settings, &request(agent, old, 46))
    else {
        panic!("ordinary goal mutation during closing refused")
    };
    let after = fixture.records();
    let added = &after[before.len()..];
    assert_eq!(
        added
            .iter()
            .filter(|record| matches!(record,Record::GoalSet {goal:changed,..} if changed.id==goal))
            .count(),
        1
    );
    assert!(
        !added
            .iter()
            .any(|record| matches!(record,Record::TurnIssued {agent:who,..} if *who==agent))
    );
    let status: ClosingStatus = serde_json::from_value(
        call(
            &settings,
            Method::SystemClosingTime,
            serde_json::to_value(ClosingParams::Status {
                lsn: started.announcement,
                after: None,
            })
            .unwrap(),
        )
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(status.total, 1);
    assert_eq!(status.participants[0].agent, participant);
    running.stop(ExitCause::Ended);
}

#[test]
fn distinct_concurrent_proposals_apply_once_and_retired_or_goal_less_targets_refuse() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let requests = [request(agent, old, 50), request(agent, old, 51)];
    let mut replies = Vec::new();
    std::thread::scope(|scope| {
        let calls: Vec<_> = requests
            .iter()
            .map(|request| {
                let settings = &settings;
                scope.spawn(move || replace(settings, request))
            })
            .collect();
        for call in calls {
            replies.push(call.join().unwrap());
        }
    });
    assert_eq!(
        replies
            .iter()
            .filter(|reply| matches!(reply, GoalReplaceResult::Changed { .. }))
            .count(),
        1
    );
    assert_eq!(
        replies
            .iter()
            .filter(|reply| matches!(
                reply,
                GoalReplaceResult::Refused {
                    reason: dudes_proto::rpc::goal::GoalReplaceRefusal::StaleGoal,
                    ..
                }
            ))
            .count(),
        1
    );
    let winner = replies
        .iter()
        .position(|reply| matches!(reply, GoalReplaceResult::Changed { .. }))
        .unwrap();
    let empty = common::hire(&running.handle, fixture.state.path());
    assert!(matches!(
        replace(&settings, &request(empty, old, 52)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::StaleGoal,
            ..
        }
    ));
    assert!(
        call(
            &settings,
            Method::AgentRetire,
            json!({"agent":agent,"reason":"owned retirement control"})
        )
        .error
        .is_none()
    );
    assert!(matches!(
        replace(&settings, &request(agent, old, 53)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::AgentAbsent,
            ..
        }
    ));
    assert_eq!(replace(&settings, &requests[winner]), replies[winner]);
    running.stop(ExitCause::Ended);
    let (_, mut restarted) = start(&fixture);
    assert_eq!(replace(&settings, &requests[winner]), replies[winner]);
    restarted.stop(ExitCause::Ended);
}

#[test]
fn every_changed_reviewed_field_conflicts_both_inflight_and_durable() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let pause = daemon.onboarding_pause();
    let mut running = common::start(daemon);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    let req = request(agent, old, 54);
    let variants: Vec<_> = (0..7)
        .map(|case| {
            let mut changed = req.clone();
            match case {
                0 => changed.text = "different text".into(),
                1 => {
                    changed.predicate = Predicate::Command {
                        argv: vec!["/usr/bin/false".into()],
                        timeout_s: 23,
                    }
                }
                2 => changed.check.label = "Different label".into(),
                3 => changed.agent = AgentId::NIL,
                4 => changed.expected_goal = GoalId::NIL,
                5 => changed.check.id = "different-check".into(),
                6 => {
                    let Predicate::Command { timeout_s, .. } = &mut changed.predicate else {
                        unreachable!()
                    };
                    *timeout_s += 1;
                }
                _ => unreachable!(),
            }
            changed
        })
        .collect();
    pause.arm(duded::runtime::OnboardingPausePoint::BeforeGoalChangePermit);
    std::thread::scope(|scope| {
        let _release = ReleaseWriter(pause.clone());
        let first = scope.spawn(|| replace(&settings, &req));
        assert!(pause.wait_reached(std::time::Duration::from_secs(5)));
        for changed in &variants {
            assert!(matches!(
                replace(&settings, changed),
                GoalReplaceResult::Refused {
                    reason: dudes_proto::rpc::goal::GoalReplaceRefusal::RequestConflict,
                    ..
                }
            ));
        }
        pause.release();
        assert!(matches!(
            first.join().unwrap(),
            GoalReplaceResult::Changed { .. }
        ));
    });
    for changed in &variants {
        assert!(matches!(
            replace(&settings, changed),
            GoalReplaceResult::Refused {
                reason: dudes_proto::rpc::goal::GoalReplaceRefusal::RequestConflict,
                ..
            }
        ));
    }
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn issued_unspawned_and_replayed_owed_work_refuse_without_cancellation_or_new_goal() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = common::UnconfirmedHost::new(true);
    let daemon = boot(
        &settings,
        common::seams(
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let issued = issued_goal(&fixture, goal);
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    let before = fixture.records();
    assert_eq!(host.children(), 0);
    assert!(matches!(
        replace(&settings, &request(agent, goal, 55)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
            ..
        }
    ));
    assert_eq!(fixture.records(), before);
    running.stop(ExitCause::Ended);
    let reissue = common::UnconfirmedHost::new(true);
    let daemon = boot(
        &settings,
        common::seams(
            reissue.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut restarted = common::start(daemon);
    restarted.handle.doctor().unwrap();
    assert_eq!(reissue.children(), 0);
    let before = fixture.records();
    assert!(matches!(
        replace(&settings, &request(agent, goal, 55)),
        GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
            ..
        }
    ));
    assert_eq!(fixture.records(), before);
    assert_eq!(
        before
            .iter()
            .filter(|record| matches!(record,Record::TurnIssued {turn,..} if *turn==issued))
            .count(),
        1
    );
    assert!(
        !before
            .iter()
            .any(|record| matches!(record,Record::TurnEnded {turn,..} if *turn==issued))
    );
    restarted.stop(ExitCause::Ended);
}

#[test]
fn queued_old_check_completion_is_settled_by_the_writer_before_admission() {
    for end in [
        CheckEnd::Verdict,
        CheckEnd::NoVerdict,
        CheckEnd::Swept,
        CheckEnd::Panic,
    ] {
        let fixture = Fixture::new();
        let settings = fixture.settings();
        let runner = Arc::new(HeldCheck {
            end,
            ..HeldCheck::default()
        });
        let (parts, host) = native_goal_seams(&fixture, Arc::clone(&runner));
        let daemon = boot(&settings, parts).unwrap();
        let pause = daemon.onboarding_pause();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let old = common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        let turn = spawned_goal(&fixture, old);
        call(&settings, Method::LoopPause, json!({"agent":agent}));
        common::end_turn_with_native(&fixture, &running.handle, &host, turn);
        runner.await_started();
        let current = common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/false"]),
        );
        let (blocker, blocker_goal) = paused_goal(&fixture, &settings, &running);
        let completion_count = running.handle.predicate_completions_queued();
        pause.arm(duded::runtime::OnboardingPausePoint::BeforeGoalChangePermit);
        let changed = std::thread::scope(|scope| {
            let _release = ReleaseWriter(pause.clone());
            let blocking = scope.spawn(|| replace(&settings, &request(blocker, blocker_goal, 60)));
            assert!(pause.wait_reached(std::time::Duration::from_secs(5)));
            let ahead = running
                .handle
                .ask_queued(duded::ports::Ask::ReplaceGoal(Box::new(request(
                    agent, current, 61,
                ))))
                .unwrap();
            runner.release();
            wait_until(
                || running.handle.predicate_completions_queued() == completion_count + 1,
                "worker completion was not queued",
            );
            let behind = running
                .handle
                .ask_queued(duded::ports::Ask::ReplaceGoal(Box::new(request(
                    agent, current, 62,
                ))))
                .unwrap();
            pause.release();
            assert!(matches!(
                blocking.join().unwrap(),
                GoalReplaceResult::Changed { .. }
            ));
            assert!(matches!(
                queued_replacement(&ahead),
                GoalReplaceResult::Refused {
                    reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
                    ..
                }
            ));
            let GoalReplaceResult::Changed { goal, .. } = queued_replacement(&behind) else {
                panic!("settled check still blocked")
            };
            goal
        });
        let entries = fixture.entries();
        let changed_at = entries
            .iter()
            .find_map(|(lsn, entry)| {
                matches!(entry.record,Record::GoalSet {ref goal,..} if goal.id==changed)
                    .then_some(*lsn)
            })
            .unwrap();
        let verdicts: Vec<_> = entries
            .iter()
            .filter_map(|(lsn, entry)| {
                matches!(entry.record,Record::PredicateResult {goal,..} if goal==old)
                    .then_some(*lsn)
            })
            .collect();
        if matches!(end, CheckEnd::Verdict) {
            assert_eq!(verdicts.len(), 1);
            assert!(verdicts[0] < changed_at);
        } else {
            assert!(verdicts.is_empty());
        }
        running.stop(ExitCause::Ended);
    }
}

#[test]
fn a_failed_predicate_thread_spawn_and_a_lost_run_never_invent_a_pass() {
    for spawn_failure in [false, true] {
        let fixture = Fixture::new();
        let settings = fixture.settings();
        let runner = Arc::new(HeldCheck::default());
        let daemon = boot(
            &settings,
            common::seams(
                SharedHost::new(),
                Arc::clone(&runner),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .unwrap();
        let failure = daemon.predicate_spawn_failure();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let old = common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        let turn = fixture.await_turn();
        call(&settings, Method::LoopPause, json!({"agent":agent}));
        if spawn_failure {
            failure.arm();
        }
        common::end_turn(&running.handle, turn);
        fixture.await_record("turn ended", |record| {
            matches!(record,Record::TurnEnded {turn:ended,..} if *ended==turn).then_some(())
        });
        if spawn_failure {
            assert!(matches!(
                replace(&settings, &request(agent, old, 63)),
                GoalReplaceResult::Changed { .. }
            ));
        } else {
            runner.await_started();
            assert!(matches!(
                replace(&settings, &request(agent, old, 63)),
                GoalReplaceResult::Refused {
                    reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
                    ..
                }
            ));
        }
        running.stop(ExitCause::Ended);
        assert!(
            !fixture
                .records()
                .iter()
                .any(|record| matches!(record,Record::PredicateResult {goal,..} if *goal==old))
        );
        let (_, mut restarted) = start(&fixture);
        if !spawn_failure {
            assert!(matches!(
                replace(&settings, &request(agent, old, 63)),
                GoalReplaceResult::Changed { .. }
            ));
        }
        restarted.stop(ExitCause::Ended);
        assert!(
            !fixture
                .records()
                .iter()
                .any(|record| matches!(record,Record::PredicateResult {goal,..} if *goal==old))
        );
    }
}

#[derive(Default)]
struct CountedChecks {
    state: std::sync::Mutex<(u64, u64)>,
    changed: std::sync::Condvar,
}
impl CountedChecks {
    fn release_through(&self, count: u64) {
        self.state.lock().unwrap().1 = count;
        self.changed.notify_all();
    }
    fn await_started(&self, count: u64) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(5), |state| {
                state.0 < count
            })
            .unwrap();
        assert_eq!(state.0, count);
    }
}
impl duded::predicate::PredicateRunner for CountedChecks {
    fn run(
        &self,
        _spec: &dudes_providers::launch::LaunchSpec,
        _timeout: std::time::Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        let ordinal = state.0;
        self.changed.notify_all();
        drop(
            self.changed
                .wait_while(state, |state| state.1 < ordinal)
                .unwrap(),
        );
        Ok(common::passes())
    }
    fn sweep(&self) {
        self.release_through(u64::MAX);
    }
}

#[test]
fn partially_settled_old_goal_runs_keep_the_owner_busy_until_the_last_completion() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let runner = Arc::new(CountedChecks::default());
    let daemon = boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let old = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let first = fixture.await_turn();
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    common::end_turn(&running.handle, first);
    runner.await_started(1);
    assert!(
        call(&settings, Method::LoopResume, json!({"agent":agent}))
            .error
            .is_none()
    );
    let second = fixture.await_record("second old-goal turn", |record| match record {
        Record::TurnIssued { turn, goal, .. } if *goal == old && *turn != first => Some(*turn),
        _ => None,
    });
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    common::end_turn(&running.handle, second);
    runner.await_started(2);
    let current = common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    let proposal = request(agent, current, 64);
    for settled in 0..=2 {
        if settled > 0 {
            runner.release_through(settled);
            wait_until(
                || running.handle.predicate_completions_queued() == settled,
                "counted predicate completion was not queued",
            );
            running.handle.doctor().unwrap();
        }
        let result = replace(&settings, &proposal);
        if settled < 2 {
            assert!(matches!(
                result,
                GoalReplaceResult::Refused {
                    reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
                    ..
                }
            ));
        } else {
            assert!(matches!(result, GoalReplaceResult::Changed { .. }));
        }
    }
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(record, Record::PredicateResult { goal, .. } if *goal == old)
            })
            .count(),
        2
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn replacement_retry_and_restart_preserve_unclaimed_and_claimed_board_cards() {
    let fixture = Fixture::new();
    let (settings, mut running) = start(&fixture);
    let (agent, old) = paused_goal(&fixture, &settings, &running);
    for title in ["Independent todo", "Claimed task"] {
        assert!(
            call(
                &settings,
                Method::BoardCreate,
                json!({"title":title,"department":"infra","deps":[]})
            )
            .error
            .is_none()
        );
    }
    let cards = call(&settings, Method::BoardList, json!({}))
        .result
        .unwrap();
    let card = cards
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["title"] == "Claimed task")
        .unwrap();
    assert!(
        call(
            &settings,
            Method::BoardClaim,
            json!({"card":card["id"],"agent":agent,"expected_seq":card["seq"]})
        )
        .error
        .is_none()
    );
    let before = call(&settings, Method::BoardList, json!({}))
        .result
        .unwrap();
    let records = fixture.records().len();
    let proposal = request(agent, old, 65);
    let result = replace(&settings, &proposal);
    assert!(matches!(result, GoalReplaceResult::Changed { .. }));
    assert_eq!(replace(&settings, &proposal), result);
    assert_eq!(
        call(&settings, Method::BoardList, json!({}))
            .result
            .unwrap(),
        before
    );
    assert!(
        !fixture.records()[records..].iter().any(|record| {
            matches!(record, Record::Claim { .. } | Record::CardTransition { .. })
        })
    );
    running.stop(ExitCause::Ended);
    let (_, mut restarted) = start(&fixture);
    assert_eq!(replace(&settings, &proposal), result);
    assert_eq!(
        call(&settings, Method::BoardList, json!({}))
            .result
            .unwrap(),
        before
    );
    restarted.stop(ExitCause::Ended);
}

#[test]
fn live_plain_and_onboarding_names_that_do_not_recompute_refuse_before_any_publication() {
    use dudes_supervisor::{Command, Input};
    for onboarding in [false, true] {
        let fixture = Fixture::new();
        let (settings, mut running) = start(&fixture);
        let (agent, old) = paused_goal(&fixture, &settings, &running);
        let proposal = request(agent, old, 66);
        let result = replace(&settings, &proposal);
        let GoalReplaceResult::Changed { goal, .. } = result else {
            panic!("valid replacement refused")
        };
        let mut copied = fixture
            .records()
            .into_iter()
            .find_map(|record| match record {
                Record::GoalSet { goal: stored, .. } if stored.id == goal => Some(stored),
                _ => None,
            })
            .unwrap();
        copied.id = GoalId::NIL;
        copied.completion_check.as_mut().unwrap().label = "Forged label".into();
        let command = if onboarding {
            Command::BrowserOnboardGoal(Box::new(dudes_supervisor::input::BrowserOnboardGoal {
                agent,
                goal: copied,
                attempt_id: Hex32::new([67; 32]),
                session_digest: Hex32::new([68; 32]),
            }))
        } else {
            Command::GoalSet {
                agent,
                text: copied.text,
                predicate: copied.predicate,
                budget: copied.budget,
                hop_cap: copied.hop_cap,
                browser_goal: None,
                goal_replacement: None,
                completion_check: copied.completion_check,
                baseline: None,
            }
        };
        let before = fixture.records();
        let projection = call(&settings, Method::AgentList, json!({}))
            .result
            .unwrap();
        running.handle.input(Input::Command(command));
        running.handle.doctor().unwrap();
        assert_eq!(fixture.records(), before);
        assert_eq!(
            call(&settings, Method::AgentList, json!({}))
                .result
                .unwrap(),
            projection
        );
        assert_eq!(replace(&settings, &proposal), result);
        assert!(
            call(&settings, Method::LoopResume, json!({"agent":agent}))
                .error
                .is_some()
        );
        running.stop(ExitCause::Ended);
    }
}

#[test]
fn replay_keeps_recorded_check_and_allowance_after_reserve_and_enrollment_change() {
    use duded::floor_actions::{Enrollment, Profile};
    use dudes_types::{IsolationMode, Tokens, Usd};
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "reviewed-case".into(),
            label: "Owned test workspace".into(),
            checks: vec![duded::floor_actions::EnrolledCheck::enrolled(
                "Reviewed case",
                Predicate::Command {
                    argv: vec!["/usr/bin/false".into()],
                    timeout_s: 23,
                },
            )],
            cwd: fixture.state.path().to_path_buf(),
            written_cwd: fixture.state.path().to_path_buf(),
            department: "infra".into(),
            isolation: IsolationMode::Default,
            agent_budget: dudes_proto::AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: dudes_proto::GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: Some(Tokens::new(100_000)),
            },
            hop_cap: 8,
            mark: None,
        }],
    };
    let daemon = boot(&settings, replay_seams(&fixture)).unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let mut source = common::goal_params(agent, &["/usr/bin/false"]);
    source.budget = settings.enrollment.profiles[0].goal_budget;
    let old = common::set_goal(&running.handle, source);
    let turn = fixture.await_turn();
    call(&settings, Method::LoopPause, json!({"agent":agent}));
    common::end_turn(&running.handle, turn);
    fixture.await_record("source check settled", |record| {
        matches!(record, Record::PredicateResult { goal, .. } if *goal == old).then_some(())
    });
    let proposal = request(agent, old, 69);
    let result = replace(&settings, &proposal);
    let GoalReplaceResult::Changed { goal, .. } = result else {
        panic!("reserved replacement refused")
    };
    let stored = fixture
        .records()
        .into_iter()
        .find(|record| matches!(record, Record::GoalSet { goal: stored, .. } if stored.id == goal))
        .unwrap();
    let Record::GoalSet {
        goal: carried,
        goal_replacement: Some(meta),
        ..
    } = &stored
    else {
        panic!("replacement attribution missing")
    };
    assert_eq!(meta.reserved, settings.per_turn_floor);
    assert_eq!(carried.budget.usd, Some(Usd::from_micros(4_999_999)));
    assert_eq!(carried.budget.tokens, Some(Tokens::new(99_999)));
    assert_eq!(carried.hop_cap, 7);
    let before = call(&settings, Method::AgentList, json!({}))
        .result
        .unwrap();
    running.stop(ExitCause::Ended);
    settings.per_turn_floor = dudes_proto::Spend {
        usd: Usd::from_micros(500_000),
        tokens: Tokens::new(20_000),
        work_tokens: Tokens::new(20_000),
    };
    settings.enrollment = Enrollment::default();
    let daemon = boot(&settings, replay_seams(&fixture)).unwrap();
    let mut restarted = common::start(daemon);
    assert_eq!(replace(&settings, &proposal), result);
    assert_eq!(
        call(&settings, Method::AgentList, json!({}))
            .result
            .unwrap(),
        before
    );
    let replacement_records: Vec<_> = fixture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                Record::GoalSet {
                    goal_replacement: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(replacement_records, [stored]);
    restarted.stop(ExitCause::Ended);
}
