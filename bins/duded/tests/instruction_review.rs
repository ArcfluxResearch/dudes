//! Managed instruction review through the production socket and an owned native child.
#![cfg(unix)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use duded::settings::FloorControl;
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::record::Hex32;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::ticket::{TicketAcceptOutcome, TicketAcceptResult, TicketReadResult};
use dudes_proto::rpc::{ErrorCode, Method, Response};
use dudes_proto::{AgentBudget, GoalBudget, Predicate, Record, TicketReason};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::EnvVar;
use dudes_providers::registry::Adapters;
use dudes_rpc::Client;
use dudes_types::{CardId, CardStatus, IsolationMode, Usd};
use serde_json::json;

struct HostStreams(Arc<duded::ChildEnds>);
struct HostTurn(duded::ChildTurn);
impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&dudes_supervisor::host::AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostTurn(self.0.watch(key)))
    }
}
impl ChildStream for HostTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
    }
    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.ended_with_elapsed(
            dudes_proto::ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

#[derive(Default)]
struct CheckGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl CheckGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

struct ReviewRunner {
    inner: common::ScriptedRunner,
    gate: Option<Arc<CheckGate>>,
}

impl duded::predicate::PredicateRunner for ReviewRunner {
    fn run(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        if let Some(gate) = &self.gate {
            gate.entered.store(true, Ordering::SeqCst);
            let (released, _) = gate
                .changed
                .wait_timeout_while(
                    gate.released.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            if !*released {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        duded::predicate::PredicateRunner::run(&self.inner, spec, timeout)
    }

    fn sweep(&self) {
        if let Some(gate) = &self.gate {
            gate.release();
        }
        duded::predicate::PredicateRunner::sweep(&self.inner);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Completion {
    Passes,
    Fails,
}

fn managed_review(changed: bool, completion: Completion, held_controls: bool, delayed: bool) {
    let completed = completion == Completion::Passes;
    let fixture = common::Fixture::new();
    let project = fixture.state.path().join("project");
    let work = fixture.state.path().join("child");
    let home = fixture.state.path().join("owned-home");
    for path in [&project, &work, &home] {
        std::fs::create_dir(path).unwrap();
    }
    let project = project.canonicalize().unwrap();
    let instruction = project.join("CLAUDE.md");
    std::fs::write(&instruction, "initial project instruction\n").unwrap();
    let command = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/two-roles/bin/claude")
        .canonicalize()
        .unwrap();
    let settings = owned_settings(&fixture, &project, &work, &home, completed, held_controls);
    let (mut running, gate) = start_review_daemon(&settings, command, completed, delayed);
    let agent = hire_shared_peers(&running, &project);
    let mut socket = Client::connect(settings.socket()).unwrap();
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .unwrap(),
        )
        .unwrap()
    };
    let mut call = |method, params| {
        socket
            .call_as_operator(method, Some(params), credential())
            .unwrap()
    };
    let (card, turn) = start_ticket(&fixture, &mut call, agent);
    let started_lsn = assert_sampled_start(&fixture, turn, &instruction);
    if changed {
        std::fs::write(&instruction, "changed project instruction\n").unwrap();
    }
    if !completed && held_controls {
        success(call(Method::LoopPause, json!({"agent":agent})));
    }
    std::fs::write(work.join("release.1"), "").unwrap();
    assert_finished_boundary(&fixture, turn, started_lsn);
    if held_controls {
        assert_completed_peer_hire(&mut call, &project);
    }
    let read = await_review(
        &fixture,
        &mut call,
        card,
        agent,
        completed,
        held_controls,
        gate.as_deref(),
    );
    assert_exact_review(
        &fixture,
        &mut call,
        card,
        &read,
        &instruction,
        changed,
        completed,
    );
    if let Some(gate) = &gate {
        assert_delayed_completion(&fixture, gate, agent, &read);
    }
    if !completed && !held_controls {
        assert_admitted_continuation(&fixture, agent, turn, &read);
    }
    if !completed && held_controls {
        assert_held_controls(&fixture, &running, agent);
    }
    drop(socket);
    running.stop(duded::ExitCause::ShutdownRpc);
}

fn owned_settings(
    fixture: &common::Fixture,
    project: &std::path::Path,
    work: &std::path::Path,
    home: &std::path::Path,
    completed: bool,
    held_controls: bool,
) -> duded::settings::Settings {
    let mut settings = fixture.settings();
    settings.daemon_env = vec![
        EnvVar::new("PATH", "/usr/bin:/bin"),
        EnvVar::new("HOME", home.as_os_str()),
        EnvVar::new("DUDES_EXAMPLE_WORK", work.as_os_str()),
        EnvVar::new("DUDES_EXAMPLE_HOLD_SECONDS", "15"),
    ];
    settings.floor_board_enabled = true;
    settings.floor_hire_enabled = false;
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: false,
        renew: false,
    };
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "owned-project".into(),
            label: "Owned project".into(),
            cwd: project.to_path_buf(),
            written_cwd: project.to_path_buf(),
            checks: vec![EnrolledCheck {
                id: "done".into(),
                label: "Owned check".into(),
                predicate: Predicate::Command {
                    argv: vec![common::true_program().into()],
                    timeout_s: 1,
                },
                may_already_pass: true,
            }],
            department: "infra".into(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(if completed || !held_controls {
                    Usd::from_dollars(5)
                } else {
                    Usd::from_micros(10_000)
                }),
                tokens: None,
            },
            hop_cap: 4,
            mark: None,
        }],
    };
    settings
}

fn start_review_daemon(
    settings: &duded::settings::Settings,
    command: std::path::PathBuf,
    completed: bool,
    delayed: bool,
) -> (common::Running, Option<Arc<CheckGate>>) {
    let predicates = if completed {
        common::ScriptedRunner::scripted([common::passes()])
    } else {
        common::ScriptedRunner::scripted([common::fails()])
    };
    let gate = delayed.then(|| Arc::new(CheckGate::default()));
    let predicates = ReviewRunner {
        inner: predicates,
        gate: gate.clone(),
    };
    let children = duded::ChildEnds::new();
    let mut seams = common::seams(
        RealHost::watching_with_grace(
            Arc::new(HostStreams(Arc::clone(&children))),
            Duration::from_millis(300),
        ),
        Arc::new(predicates),
        Arc::new(common::TestClock::new()),
        duded::startup::Witness::default(),
    );
    seams.children = children;
    seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(ClaudeConfig {
        command: command.into_os_string(),
        ..ClaudeConfig::default()
    })) as Box<dyn Adapter>]);
    let daemon = duded::boot(settings, seams).unwrap();
    let running = common::start(daemon);
    (running, gate)
}

fn hire_shared_peers(running: &common::Running, project: &std::path::Path) -> dudes_types::AgentId {
    let hire = || match running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(
            dudes_proto::rpc::agent::HireParams {
                trusted: true,
                ..common::hire_params(project)
            },
        )))
        .unwrap()
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("unexpected hire answer: {other:?}"),
    };
    let peer = hire();
    let agent = hire();
    assert_ne!(
        peer, agent,
        "an idle recorded peer makes this project shared"
    );
    agent
}

fn success(response: Response) -> serde_json::Value {
    assert!(response.error.is_none(), "{:?}", response.error);
    response.result.unwrap()
}

fn start_ticket(
    fixture: &common::Fixture,
    call: &mut impl FnMut(Method, serde_json::Value) -> Response,
    agent: dudes_types::AgentId,
) -> (CardId, dudes_types::TurnId) {
    let created = success(call(
        Method::TicketCreate,
        json!({"title":"Owned instruction interval"}),
    ));
    let card: CardId = serde_json::from_value(created["card"].clone()).unwrap();
    success(call(
        Method::TicketAssign,
        json!({"card":card,"agent":agent,"expected_seq":0}),
    ));
    let turn = fixture.await_record("a real managed child", |record| match record {
        Record::TurnSpawned {
            agent: owner, turn, ..
        } if *owner == agent => Some(*turn),
        _ => None,
    });
    (card, turn)
}

fn assert_sampled_start(
    fixture: &common::Fixture,
    turn: dudes_types::TurnId,
    instruction: &std::path::Path,
) -> dudes_types::Lsn {
    let entries = fixture.entries();
    let started = entries
        .iter()
        .find_map(|(lsn, entry)| match &entry.record {
            Record::InstructionObservationStarted {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: observed, .. },
                snapshot,
                ..
            } if *observed == turn => Some((*lsn, snapshot)),
            _ => None,
        })
        .unwrap();
    let issue = entries.iter().find(|(_, entry)| matches!(&entry.record, Record::TurnIssued { turn: issued, .. } if *issued == turn)).unwrap().0;
    assert!(
        started.0 < issue,
        "the actual fingerprint baseline is durable before issue"
    );
    assert!(
        started
            .1
            .inputs
            .iter()
            .any(|input| input.path.to_path().ok().as_deref() == Some(instruction)),
        "the trusted actual composer samples its project file; gaps: {:?}; inputs: {:?}",
        started.1.gaps,
        started
            .1
            .inputs
            .iter()
            .map(|input| &input.path)
            .collect::<Vec<_>>()
    );
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::InstructionAttemptBoundarySettled { started_lsn, .. } if *started_lsn == started.0)),
        "a live native target has no durable absence receipt");
    started.0
}

fn assert_finished_boundary(
    fixture: &common::Fixture,
    turn: dudes_types::TurnId,
    baseline_lsn: dudes_types::Lsn,
) {
    fixture.await_record("the actual ending comparison", |record| match record {
        Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed { turn: observed, .. },
            ..
        } if *observed == turn => Some(()),
        _ => None,
    });
    fixture.await_record("durable scoped absence of the real completed native peer", |record| match record {
        Record::InstructionAttemptBoundarySettled { owner, started_lsn, proof: dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned { .. } } }
            if *started_lsn == baseline_lsn && matches!(owner, dudes_proto::InstructionObservationOwner::Managed { turn: observed, .. } if *observed == turn) => Some(()),
        _ => None,
    });
}

fn assert_completed_peer_hire(
    call: &mut impl FnMut(Method, serde_json::Value) -> Response,
    project: &std::path::Path,
) {
    let admitted = success(call(
        Method::AgentHire,
        serde_json::to_value(dudes_proto::rpc::agent::HireParams {
            name: "Completed peer control".into(),
            trusted: true,
            isolation: Some(IsolationMode::Inherit),
            ..common::hire_params(project)
        })
        .unwrap(),
    ));
    assert!(
        admitted.get("agent").is_some(),
        "a completed native peer permits a new inherit hire"
    );
}

fn await_review(
    fixture: &common::Fixture,
    call: &mut impl FnMut(Method, serde_json::Value) -> Response,
    card: CardId,
    agent: dudes_types::AgentId,
    completed: bool,
    held_controls: bool,
    gate: Option<&CheckGate>,
) -> TicketReadResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read: TicketReadResult =
            serde_json::from_value(success(call(Method::TicketRead, json!({"card":card}))))
                .unwrap();
        if read.card.status == CardStatus::Blocked
            && read
                .ticket
                .instruction_review
                .as_ref()
                .is_some_and(|review| !review.pending_comparison)
        {
            let settled = fixture.records().iter().any(|record| matches!(record, Record::LoopDecided { agent: owner, decision, .. } if *owner == agent && if completed { matches!(decision, dudes_proto::LoopDecision::Done) } else { matches!(decision, dudes_proto::LoopDecision::Hold { reason: dudes_proto::HoldReason::SharedInstructionsReview }) }));
            if (!completed && held_controls)
                || settled
                || gate.is_some_and(|gate| gate.entered.load(Ordering::SeqCst))
            {
                break read;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the durable comparison and completion settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_exact_review(
    fixture: &common::Fixture,
    call: &mut impl FnMut(Method, serde_json::Value) -> Response,
    card: CardId,
    read: &TicketReadResult,
    instruction: &std::path::Path,
    changed: bool,
    completed: bool,
) {
    let review = read.ticket.instruction_review.as_ref().unwrap();
    assert!(
        review
            .gaps
            .iter()
            .any(dudes_proto::InstructionCoverageGap::holds),
        "the idle recorded peer is an observed gap, which holds the ticket"
    );
    assert_eq!(
        review
            .changed_paths
            .iter()
            .any(|path| path.path.to_path().ok().as_deref() == Some(instruction)),
        changed
    );
    assert_eq!(
        read.ticket.reason,
        Some(if changed {
            TicketReason::SharedInstructionsChanged
        } else {
            TicketReason::SharedInstructionsUnknown
        })
    );
    let reviews_before = fixture.records().iter().filter(|record| matches!(record, Record::TicketInstructionReviewed { card: reviewed, .. } if *reviewed == card)).count();
    let missing = call(
        Method::TicketAccept,
        json!({"card":card,"expected_seq":read.card.seq}),
    );
    assert_eq!(
        missing.error.unwrap().data.unwrap()["reason"],
        "instruction_review_required"
    );
    let mut stale_watermark = review.watermark;
    stale_watermark.set_digest = Hex32::new([7; 32]);
    let stale = call(
        Method::TicketAccept,
        json!({"card":card,"expected_seq":read.card.seq,"review_watermark":stale_watermark}),
    );
    assert_eq!(stale.error.unwrap().code, ErrorCode::Conflict.code());
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::TicketInstructionReviewed { card: reviewed, .. } if *reviewed == card)).count(), reviews_before, "refused review appends no review record");
    let unchanged: TicketReadResult =
        serde_json::from_value(success(call(Method::TicketRead, json!({"card":card})))).unwrap();
    assert_eq!(unchanged.card.seq, read.card.seq);
    assert_eq!(
        unchanged
            .ticket
            .instruction_review
            .as_ref()
            .unwrap()
            .watermark,
        review.watermark
    );
    let result = success(call(
        Method::TicketAccept,
        json!({"card":card,"expected_seq":read.card.seq,"review_watermark":review.watermark}),
    ));
    let accepted: TicketAcceptResult = serde_json::from_value(result.clone()).unwrap();
    if completed {
        assert_eq!(result, json!({}));
    } else {
        assert_eq!(
            accepted.outcome,
            Some(TicketAcceptOutcome::InstructionsAcknowledged)
        );
    }
    let after: TicketReadResult =
        serde_json::from_value(success(call(Method::TicketRead, json!({"card":card})))).unwrap();
    assert!(
        after
            .ticket
            .instruction_review
            .as_ref()
            .is_none_or(|current| current.watermark != review.watermark)
    );
    assert_eq!(
        after.card.status,
        if completed {
            CardStatus::Done
        } else {
            CardStatus::Doing
        }
    );
}

fn assert_delayed_completion(
    fixture: &common::Fixture,
    gate: &CheckGate,
    agent: dudes_types::AgentId,
    read: &TicketReadResult,
) {
    assert!(gate.entered.load(Ordering::SeqCst));
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::TurnIssued { agent: owner, .. } if *owner == agent)).count(), 1, "acknowledgment must wait for the observed completion check");
        std::thread::sleep(Duration::from_millis(10));
    }
    gate.release();
    fixture.await_record(
        "the delayed observed completion check",
        |record| match record {
            Record::PredicateResult { goal, .. } if Some(*goal) == read.ticket.goal => Some(()),
            _ => None,
        },
    );
}

fn assert_admitted_continuation(
    fixture: &common::Fixture,
    agent: dudes_types::AgentId,
    turn: dudes_types::TurnId,
    read: &TicketReadResult,
) {
    let next = fixture.await_record(
        "ordinary admitted continuation after exact review",
        |record| match record {
            Record::TurnIssued {
                agent: owner,
                turn: next,
                goal,
                ..
            } if *owner == agent && *next != turn && Some(*goal) == read.ticket.goal => Some(*next),
            _ => None,
        },
    );
    assert_ne!(next, turn);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Resume { agent: owner, .. } if *owner == agent))
    );
}

fn assert_held_controls(
    fixture: &common::Fixture,
    running: &common::Running,
    agent: dudes_types::AgentId,
) {
    let duded::ports::Answered::Agents(agents) =
        running.handle.ask(duded::ports::Ask::AgentList).unwrap()
    else {
        panic!("agent views");
    };
    let view = agents.iter().find(|view| view.id == agent).unwrap();
    assert!(view.paused);
    let goal = view.goal.as_ref().unwrap();
    assert_eq!(goal.budget.usd, Some(Usd::from_micros(10_000)));
    assert_eq!(goal.spent.usd, Usd::from_micros(10_000));
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Pause { agent: owner, .. } if *owner == agent))
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Resume { agent: owner, .. } if *owner == agent))
    );
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::TurnIssued { agent: owner, .. } if *owner == agent)).count(), 1);
}

#[test]
fn a_real_managed_edit_requires_exact_review_and_acknowledgment_keeps_pause() {
    managed_review(true, Completion::Fails, true, false);
}

#[test]
fn an_unchanged_actual_claude_composer_reports_coverage_unknown_and_preserves_completed_accept() {
    managed_review(false, Completion::Passes, true, false);
}

#[test]
fn healthy_unpaused_work_continues_through_ordinary_admission_after_review() {
    managed_review(true, Completion::Fails, false, false);
}

#[test]
fn acknowledgment_waits_for_an_inflight_observed_completion_check() {
    managed_review(true, Completion::Fails, false, true);
}

/// An agent working beside an idle peer, so the project is shared, rewrites one of the engine's
/// project settings files while its ticket runs. The ticket holds with the file listed.
fn settings_edit_in_a_shared_project(edited: &str) {
    let fixture = common::Fixture::new();
    let project = fixture.state.path().join("project");
    let work = fixture.state.path().join("child");
    let home = fixture.state.path().join("owned-home");
    for path in [&project, &work, &home] {
        std::fs::create_dir(path).unwrap();
    }
    let project = project.canonicalize().unwrap();
    std::fs::write(project.join("CLAUDE.md"), "initial project instruction\n").unwrap();
    let target = project.join(edited);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "{}\n").unwrap();
    let command = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/two-roles/bin/claude")
        .canonicalize()
        .unwrap();
    let settings = owned_settings(&fixture, &project, &work, &home, true, false);
    let (mut running, _) = start_review_daemon(&settings, command, true, false);
    let agent = hire_shared_peers(&running, &project);
    let mut socket = Client::connect(settings.socket()).unwrap();
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .unwrap(),
        )
        .unwrap()
    };
    let mut call = |method, params| {
        socket
            .call_as_operator(method, Some(params), credential())
            .unwrap()
    };
    let (card, turn) = start_ticket(&fixture, &mut call, agent);
    let started_lsn = assert_sampled_start(&fixture, turn, &target);
    std::fs::write(&target, "{\"changed\":true}\n").unwrap();
    std::fs::write(work.join("release.1"), "").unwrap();
    assert_finished_boundary(&fixture, turn, started_lsn);
    let read = await_review(&fixture, &mut call, card, agent, true, false, None);
    assert_eq!(
        read.ticket.reason,
        Some(TicketReason::SharedInstructionsChanged)
    );
    let review = read.ticket.instruction_review.as_ref().unwrap();
    assert!(
        review
            .changed_paths
            .iter()
            .any(|path| path.path.to_path().ok().as_deref() == Some(target.as_path()))
    );
    drop(socket);
    running.stop(duded::ExitCause::ShutdownRpc);
}

#[test]
fn editing_claudes_project_settings_holds_the_ticket() {
    settings_edit_in_a_shared_project(".claude/settings.json");
}

#[test]
fn editing_claudes_local_project_settings_holds_the_ticket() {
    settings_edit_in_a_shared_project(".claude/settings.local.json");
}
