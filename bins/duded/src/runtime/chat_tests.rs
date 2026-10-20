use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use dudes_proto::{ChatReason, LoopDecision, Record};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::registry::Adapters;
use dudes_types::{AgentId, Lsn, TurnId};

use crate::floor_chat::{ChatOutcome, ChatRequest, ChatVerb};
use crate::floor_session::{FloorSessions, SessionIdentity};
use crate::settings::RunOptions;
use crate::{ExitCause, Handle, boot_with_options};

#[derive(Clone, Default)]
struct HostEntry(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl HostEntry {
    fn wait(&self) -> bool {
        let state = self.0.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .1
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.0)
            .unwrap_or_else(PoisonError::into_inner);
        state.0
    }

    fn release(&self) {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.0.1.notify_all();
    }
}

struct ReleaseHost(HostEntry);
impl Drop for ReleaseHost {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone)]
struct BoundaryHost {
    inner: common::SharedHost,
    calls: Arc<AtomicUsize>,
    unknown: Arc<AtomicBool>,
    not_launched: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    entry: HostEntry,
}

impl BoundaryHost {
    fn new() -> Self {
        Self::with_host(common::SharedHost::new())
    }

    fn with_native_evidence(journal: std::path::PathBuf) -> Self {
        Self::with_host(common::SharedHost::with_native_evidence(journal))
    }

    fn with_host(inner: common::SharedHost) -> Self {
        Self {
            inner,
            calls: Arc::default(),
            unknown: Arc::default(),
            not_launched: Arc::default(),
            held: Arc::default(),
            entry: HostEntry::default(),
        }
    }
}

impl dudes_supervisor::ProcessHost for BoundaryHost {
    fn spawn(
        &mut self,
        spec: &dudes_providers::launch::LaunchSpec,
        install: &dudes_providers::adapter::ManagedInstall,
    ) -> dudes_supervisor::SpawnResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.held.swap(false, Ordering::SeqCst) {
            let mut state = self
                .entry
                .0
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.0 = true;
            self.entry.0.1.notify_all();
            let (state, _) = self
                .entry
                .0
                .1
                .wait_timeout_while(state, Duration::from_secs(5), |state| !state.1)
                .unwrap_or_else(PoisonError::into_inner);
            assert!(state.1, "owned host entry hold expired");
        }
        if self.not_launched.swap(false, Ordering::SeqCst) {
            dudes_supervisor::SpawnResult::NotLaunched {
                reason: std::io::Error::other("owned host confirms no child"),
            }
        } else if self.unknown.swap(false, Ordering::SeqCst) {
            dudes_supervisor::SpawnResult::CleanupUnconfirmed {
                reason: std::io::Error::other("stand-in dispatch outcome is unresolved"),
            }
        } else {
            self.inner.spawn(spec, install)
        }
    }
    fn kill(&mut self, key: &dudes_supervisor::AdoptionKey) -> dudes_supervisor::KillOutcome {
        self.inner.kill(key)
    }
    fn alive(&self, key: &dudes_supervisor::AdoptionKey) -> dudes_supervisor::Liveness {
        self.inner.alive(key)
    }
    fn owned_targets(&self, limit: usize) -> dudes_supervisor::host::OwnedTargets {
        self.inner.owned_targets(limit)
    }
    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &dudes_supervisor::AdoptionKey,
        receipt: &dudes_supervisor::host::NativeBoundaryReceipt,
    ) -> dudes_supervisor::host::NativeBoundaryAcknowledgement {
        self.inner.acknowledge_native_boundary_absence(key, receipt)
    }
    fn boot_identity(&self) -> dudes_supervisor::HostBoot {
        self.inner.boot_identity()
    }
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.entry.release();
        self.inner.shutdown()
    }
}

use super::work_test_support as common;

#[cfg(unix)]
#[path = "../../tests/common/native_chat_host.rs"]
mod native_chat_host;

fn session() -> SessionIdentity {
    session_with_clock().0
}

fn session_with_clock() -> (SessionIdentity, Arc<common::TestClock>) {
    let clock = Arc::new(common::TestClock::new());
    (session_from_clock(Arc::clone(&clock)), clock)
}

fn session_from_clock<C: dudes_supervisor::Clock + Send + Sync + 'static>(
    clock: Arc<C>,
) -> SessionIdentity {
    let sessions = FloorSessions::new("127.0.0.1:31337".parse().unwrap(), clock);
    let issued = sessions.issue().expect("owned test bootstrap");
    let exchange = sessions
        .exchange(
            "127.0.0.1:31337",
            "http://127.0.0.1:31337",
            issued.bootstrap.as_bytes(),
        )
        .expect("owned exchange");
    sessions
        .authorize_onboard(
            "127.0.0.1:31337",
            "http://127.0.0.1:31337",
            &exchange.cookie,
            &exchange.csrf,
        )
        .expect("live session")
}

struct PermitClock {
    inner: common::TestClock,
    expire_after_read: AtomicBool,
}

impl dudes_supervisor::Clock for PermitClock {
    fn now(&self) -> dudes_types::Timestamp {
        let now = dudes_supervisor::Clock::now(&self.inner);
        if self.expire_after_read.swap(false, Ordering::SeqCst) {
            self.inner.advance(48 * 60 * 60 * 1_000);
        }
        now
    }
}

fn request(verb: ChatVerb, agent: AgentId, value: &str, identity: u8) -> ChatRequest {
    let mut key = "A".repeat(43);
    key.replace_range(..1, &char::from(b'A' + identity).to_string());
    let field = match verb {
        ChatVerb::Send => "text",
        ChatVerb::Cancel => "note_lsn",
    };
    let body = serde_json::json!({"request_id":key,"agent":agent.to_string(),field:value});
    ChatRequest::parse(verb, &serde_json::to_vec(&body).unwrap()).expect("canonical request")
}

fn send(handle: &Handle, identity: &SessionIdentity, agent: AgentId, text: &str, key: u8) -> Lsn {
    let ChatOutcome::Queued { note_lsn, .. } = handle
        .chat(identity.clone(), request(ChatVerb::Send, agent, text, key))
        .expect("durable send")
    else {
        panic!("expected queued note");
    };
    note_lsn
}

fn with_owned_program<H: dudes_supervisor::ProcessHost, P: crate::predicate::PredicateRunner>(
    settings: &crate::Settings,
    host: H,
    predicates: Arc<P>,
) -> crate::Seams<H, P, common::TestClock, common::FixtureRng> {
    let mut seams = common::seams(
        settings,
        host,
        predicates,
        Arc::new(common::TestClock::new()),
        crate::Witness::default(),
    );
    seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(ClaudeConfig {
        command: std::env::current_exe()
            .expect("owned test program")
            .into_os_string(),
        ..ClaudeConfig::default()
    })) as Box<dyn Adapter>]);
    seams
}

fn with_native_program<H: dudes_supervisor::ProcessHost, P: crate::predicate::PredicateRunner>(
    settings: &crate::Settings,
    host: H,
    native: &common::SharedHost,
    predicates: Arc<P>,
) -> crate::Seams<H, P, common::TestClock, common::FixtureRng> {
    let seams = with_owned_program(settings, host, predicates);
    native.bind_child_ends(Arc::clone(&seams.children));
    seams
}

#[cfg(unix)]
#[derive(Debug)]
struct EchoEngine(std::path::PathBuf);

#[cfg(unix)]
impl Adapter for EchoEngine {
    fn provider(&self) -> dudes_types::Provider {
        dudes_types::Provider::Claude
    }
    fn program(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
    fn tier(&self) -> dudes_types::Tier {
        dudes_types::Tier::HookGoverned
    }
    fn isolation(&self) -> dudes_providers::adapter::Isolation {
        dudes_providers::adapter::Isolation::None
    }
    fn session_acquisition(&self) -> dudes_providers::adapter::SessionAcquisition {
        dudes_providers::claude::SESSION_ACQUISITION
    }
    fn model_selection(&self) -> dudes_types::ModelSelection {
        dudes_types::ModelSelection::provider_default()
    }
    fn managed(
        &self,
        ctx: &dudes_providers::adapter::AgentCtx<'_>,
        task: &str,
        _: &dudes_providers::adapter::TurnSession,
        _: &dudes_types::ModelSelection,
    ) -> Result<dudes_providers::launch::LaunchSpec, dudes_providers::launch::LaunchError> {
        let mut env = ctx.daemon_env.to_vec();
        env.push(dudes_providers::launch::EnvVar::new(
            "DUDES_OWNED_CHAT_ECHO",
            "1",
        ));
        Ok(dudes_providers::launch::LaunchSpec {
            argv: vec![
                self.0.as_os_str().to_owned(),
                "--exact".into(),
                "runtime::chat_tests::owned_chat_echo".into(),
                "--nocapture".into(),
                "--quiet".into(),
            ],
            env,
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::WriteThenClose(task.as_bytes().to_vec()),
        })
    }
    fn turn_reader(&self) -> Box<dyn dudes_providers::stream::TurnReader> {
        Box::new(dudes_providers::claude::stream::TurnStream::new())
    }
    fn managed_install(
        &self,
        _: &dudes_providers::adapter::AgentCtx<'_>,
        _: &dudes_types::ModelSelection,
    ) -> dudes_providers::adapter::ManagedInstall {
        dudes_providers::adapter::ManagedInstall::default()
    }
    fn usage_source(&self) -> dudes_providers::adapter::UsageSource {
        dudes_providers::adapter::UsageSource::HeadlessResult
    }
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

#[cfg(unix)]
#[test]
fn owned_chat_echo() {
    use std::io::{Read as _, Write as _};
    if std::env::var_os("DUDES_OWNED_CHAT_ECHO").is_none() {
        return;
    }
    let mut task = String::new();
    std::io::stdin()
        .read_to_string(&mut task)
        .expect("managed task at launch");
    let total = if task == "café\nexact owned reply"
        || dudes_proto::recognize_taught_note_task(&task) == Some("café\nexact owned reply")
    {
        0.5
    } else {
        0.25
    };
    let init = serde_json::json!({"type":"system","subtype":"init","session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","tools":[]});
    let result = serde_json::json!({"type":"result","subtype":"success","is_error":false,
        "session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","result":format!("reply: {task}"),
        "total_cost_usd":total,"modelUsage":{"claude-opus-4":{"inputTokens":1,"outputTokens":1,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":total}}});
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{init}").expect("owned engine session announcement");
    writeln!(stdout, "{result}").expect("owned engine result");
    stdout.flush().expect("owned engine flush");
    std::process::exit(0);
}

fn issued_note(fixture: &common::Fixture, note: Lsn) -> TurnId {
    fixture.await_record("browser issue", |record| match record {
        Record::TurnIssuedWithBrowserNote { turn, note_lsn, .. }
        | Record::NoteOnlyTurnIssued { turn, note_lsn, .. }
        | Record::ConversationTurnIssued { turn, note_lsn, .. }
        | Record::TaughtNoteOnlyTurnIssued { turn, note_lsn, .. }
            if *note_lsn == note =>
        {
            Some(*turn)
        }
        _ => None,
    })
}

#[expect(
    clippy::print_stderr,
    reason = "the receipt records the exact acknowledged-prefix denominator without exporting journal contents"
)]
fn assert_acknowledged_prefixes(fixture: &common::Fixture) {
    use std::io::{Read as _, Seek as _};

    let scratch = tempfile::tempdir().unwrap();
    let mut live = dudes_journal::Projections::open(&scratch.path().join("live.redb")).unwrap();
    let mut world = dudes_supervisor::World::default();
    let entries = fixture.entries();
    for (index, (lsn, entry)) in entries.iter().enumerate() {
        let segment = dudes_journal::segment::segment_path(&fixture.journal_dir(), lsn.segment());
        let mut file = std::fs::File::open(&segment).unwrap();
        file.seek(std::io::SeekFrom::Start(lsn.offset())).unwrap();
        let mut header = [0; dudes_journal::frame::HEADER_LEN];
        file.read_exact(&mut header).unwrap();
        let header = dudes_journal::frame::Header::parse(&header);
        live.apply(*lsn, header.crc, entry).unwrap();
        world.apply(*lsn, entry);

        let cut = tempfile::tempdir().unwrap();
        for number in dudes_journal::segment::list_segments(&fixture.journal_dir()).unwrap() {
            if number > lsn.segment() {
                break;
            }
            let source = dudes_journal::segment::segment_path(&fixture.journal_dir(), number);
            let destination = dudes_journal::segment::segment_path(cut.path(), number);
            std::fs::copy(source, &destination).unwrap();
            if number == lsn.segment() {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(destination)
                    .unwrap()
                    .set_len(lsn.offset() + u64::try_from(header.frame_len()).unwrap())
                    .unwrap();
            }
        }
        let (reader, report) = dudes_journal::Reader::open(cut.path()).unwrap();
        assert!(report.torn.is_none(), "acknowledged prefix is intact");
        let rebuilt_path = scratch.path().join(format!("rebuilt-{index}.redb"));
        let mut rebuilt = dudes_journal::Projections::open(&rebuilt_path).unwrap();
        let replay = dudes_journal::replay(&reader, &mut rebuilt).unwrap();
        assert_eq!(replay.applied, u64::try_from(index + 1).unwrap());
        assert_eq!(rebuilt.export().unwrap(), live.export().unwrap());
        let mut replayed_world = dudes_supervisor::World::default();
        for row in reader.iter_from(Lsn::ZERO).unwrap() {
            let (position, entry) = row.unwrap();
            replayed_world.apply(position, &entry);
        }
        assert_eq!(format!("{replayed_world:?}"), format!("{world:?}"));
    }
    eprintln!("acknowledged daemon prefixes: {}", entries.len());
}

fn owned_spawn_key(fixture: &common::Fixture, turn: TurnId) -> dudes_supervisor::AdoptionKey {
    fixture.await_record("the exact native child", |record| match record {
        Record::TurnSpawned {
            turn: seen,
            pid,
            start_time,
            boot,
            ..
        } if *seen == turn => Some(dudes_supervisor::AdoptionKey {
            pid: *pid,
            start_time: *start_time,
            boot: boot.clone(),
        }),
        _ => None,
    })
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one real daemon cancellation lifecycle joins held readiness, durable abandonment, and closing"
)]
fn cancellation_completes_while_preparation_is_held_and_late_readiness_is_inert() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let pause = daemon.browser_launch_pause();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let ordinary = fixture.await_turn();
    let identity = session();
    let note = send(&running.handle, &identity, agent, "keep this exact note", 0);
    assert_eq!(host.spawned().len(), 1, "mid-turn send cannot dispatch");
    pause.arm();
    common::end_turn(&running.handle, ordinary);
    assert!(
        pause.wait(Duration::from_secs(5)),
        "real writer must yield held preparation"
    );
    let turn = issued_note(&fixture, note);
    let before = fixture.records();
    let mut before_world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        before_world.apply(lsn, &entry);
    }
    assert_eq!(before_world.goals[&goal].turns_on_goal, 2);
    let outcome = running
        .handle
        .chat(
            identity.clone(),
            request(ChatVerb::Cancel, agent, &note.0.to_string(), 1),
        )
        .expect("cancel while held");
    assert!(matches!(outcome, ChatOutcome::Cancelled { note_lsn, .. } if note_lsn == note));
    assert_eq!(host.spawned().len(), 1, "held preparation starts no child");
    pause.release();
    assert!(
        pause.wait_returned(1, Duration::from_secs(5)),
        "stale callback must actually return"
    );
    running.handle.doctor().expect("late readiness barrier");
    let records = fixture.records();
    assert_eq!(
        host.spawned().len(),
        1,
        "cancelled callback cannot dispatch"
    );
    assert!(records.iter().any(|record| matches!(record, Record::BrowserNoteIssuedCancelled { turn: cancelled, .. } if *cancelled == turn)));
    assert!(!records.iter().any(|record| matches!(record,
        Record::TurnEnded { turn: ended, .. } | Record::TurnSpawned { turn: ended, .. }
        | Record::BrowserNoteDispatchIntent { turn: ended, .. } if *ended == turn)));
    assert_eq!(records.iter().filter(|record| matches!(record, Record::TurnIssuedWithBrowserNote { note_lsn, .. } if *note_lsn == note)).count(), 1);
    assert_eq!(
        before
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { .. }))
            .count(),
        records
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { .. }))
            .count()
    );
    assert!(
        running
            .handle
            .closing_call(crate::closing::Request::Announce)
            .is_ok(),
        "clean abandonment cannot block closing"
    );
    running.stop(ExitCause::Ended);
    assert_eq!(host.shutdowns(), 1);
    let mut after_world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        after_world.apply(lsn, &entry);
    }
    assert_eq!(
        after_world.goals[&goal].turns_on_goal, 2,
        "cancel cannot refund admission hop"
    );
    assert_eq!(
        after_world.turns[&turn].task,
        before_world.turns[&turn].task
    );
    assert_eq!(
        after_world.agents[&agent].last_turn_end,
        before_world.agents[&agent].last_turn_end
    );
    let projection =
        dudes_journal::Projections::open(&fixture.state.path().join(crate::boot::PROJECTION_FILE))
            .unwrap();
    let day = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
    assert_eq!(
        projection
            .spend_views(agent, goal, day)
            .unwrap()
            .goal_unknown_ends,
        1,
        "only positively unused note reserve is released"
    );
    assert_acknowledged_prefixes(&fixture);
}

#[test]
fn ordinary_done_precedes_note_only_and_note_end_never_judges_again() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let predicates = Arc::new(common::ScriptedRunner::scripted([
        crate::predicate::PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: String::new(),
            timed_out: false,
        },
    ]));
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(&settings, host.clone(), &host, Arc::clone(&predicates)),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    let ordinary = fixture.await_turn();
    let note = send(
        &running.handle,
        &session(),
        agent,
        "one completed-goal note",
        0,
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, ordinary);
    let turn = issued_note(&fixture, note);
    fixture.await_record("note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
            .then_some(())
    });
    let entries = fixture.entries();
    let done = entries.iter().find_map(|(lsn, entry)| matches!(entry.record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if owner == agent).then_some(*lsn)).expect("ordinary Done");
    let admission = entries
        .iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::TaughtNoteTurnDecided {
                note_lsn,
                completion_lsn,
                ..
            } if note_lsn == note => Some((*lsn, completion_lsn)),
            _ => None,
        })
        .expect("separate note admission");
    assert!(done < admission.0);
    assert_eq!(done, admission.1);
    assert_eq!(
        host.spawned()[1].argv.last().and_then(|part| part.to_str()),
        Some(
            dudes_proto::taught_note_task("one completed-goal note")
                .unwrap()
                .as_str()
        )
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    running.handle.doctor().expect("note end barrier");
    assert_eq!(predicates.runs(), 1, "note-only end skips predicate");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::LoopDecided {
                    decision: LoopDecision::Done,
                    ..
                }
            ))
            .count(),
        1
    );
    let crate::ports::Answered::Agents(agents) = running
        .handle
        .ask(crate::ports::Ask::AgentList)
        .expect("agent projection")
    else {
        panic!("expected agents");
    };
    let agent_row = agents.into_iter().find(|row| row.id == agent).unwrap();
    assert_eq!(agent_row.status, dudes_types::AgentStatus::Done);
    assert!(fixture.records().iter().any(|record| matches!(record, Record::TaughtNoteOnlyTurnIssued { goal: original, .. } if *original == goal)));
    running.stop(ExitCause::Ended);
    assert_eq!(
        predicates.runs(),
        1,
        "owned predicate threads have joined before the final count"
    );
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::PredicateResult { goal: judged, .. } if *judged == goal)).count(), 1);
    assert_acknowledged_prefixes(&fixture);
}

#[test]
fn completed_note_preserves_original_budget_question_and_accounts_hops() {
    for gate in ["budget", "hops", "question"] {
        let fixture = common::Fixture::new();
        let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
        let predicates = Arc::new(common::ScriptedRunner::scripted([common::passes()]));
        let settings = fixture.settings();
        let daemon = boot_with_options(
            &settings,
            with_native_program(&settings, host.clone(), &host, Arc::clone(&predicates)),
            RunOptions::default(),
        )
        .unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let mut params = common::goal_params(agent, &[common::true_program()]);
        if gate == "budget" {
            params.budget.usd = Some(dudes_types::Usd::from_micros(1));
        } else if gate == "hops" {
            params.hop_cap = 1;
        }
        let goal = common::set_goal(&running.handle, params);
        common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
        fixture.await_record("ordinary Done", |record| matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(()));
        if gate == "question" {
            running
                .handle
                .ask(crate::ports::Ask::Ask {
                    agent,
                    text: "still need a human answer".to_owned(),
                })
                .unwrap();
        }
        let note = send(
            &running.handle,
            &session(),
            agent,
            "gated completed note",
            0,
        );
        let admission = fixture.await_record("completed note refusal", |record| match record {
            Record::TaughtNoteTurnDecided {
                note_lsn, decision, ..
            } if *note_lsn == note => Some(decision.clone()),
            _ => None,
        });
        assert!(
            match gate {
                "budget" => matches!(
                    admission,
                    dudes_proto::NoteAdmission::Exhausted {
                        reason: dudes_proto::ExhaustedReason::GoalBudget
                    }
                ),
                "hops" => matches!(admission, dudes_proto::NoteAdmission::Issue { .. }),
                _ => matches!(admission, dudes_proto::NoteAdmission::Wait { .. }),
            },
            "completed {gate} admission has its required outcome"
        );
        if gate == "hops" {
            let turn = issued_note(&fixture, note);
            fixture.await_record("completed cap dispatch", |record| {
                matches!(record,Record::TurnSpawned{turn:spawned,..} if *spawned==turn)
                    .then_some(())
            });
        } else if gate == "budget" {
            fixture.await_record("acknowledged dependent budget audit", |record| {
                matches!(record,Record::BudgetDecision{agent:owner,..} if *owner==agent)
                    .then_some(())
            });
        }
        running.handle.doctor().unwrap();
        let before = fixture.entries().len();
        running.handle.input(dudes_supervisor::Input::Tick);
        running.handle.doctor().unwrap();
        assert_eq!(
            fixture.entries().len(),
            before,
            "unchanged refusal digest appends nothing"
        );
        assert_eq!(host.spawned().len(), if gate == "hops" { 2 } else { 1 });
        assert_eq!(predicates.runs(), 1);
        let mut world = dudes_supervisor::World::default();
        for (lsn, entry) in fixture.entries() {
            world.apply(lsn, &entry);
        }
        assert!(world.completed_goal_lsn(goal).is_some());
        assert_eq!(
            world.goals[&goal].turns_on_goal,
            if gate == "hops" { 2 } else { 1 }
        );
        let state = &world.browser_note(note).unwrap().state;
        assert!(match gate {
            "hops" => matches!(
                state,
                dudes_supervisor::world::BrowserNoteState::Delivered { .. }
            ),
            _ => matches!(state, dudes_supervisor::world::BrowserNoteState::Queued),
        });
        running.stop(ExitCause::Ended);
    }
}

#[test]
fn completed_note_replay_keeps_the_latest_inferred_end_gate() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host,
            Arc::new(common::ScriptedRunner::scripted([common::passes()])),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    fixture.await_record("ordinary Done", |record| matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(()));
    let first = send(
        &running.handle,
        &session(),
        agent,
        "historical note turn",
        0,
    );
    let turn = issued_note(&fixture, first);
    fixture.await_record("historical note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
            .then_some(())
    });
    running.stop(ExitCause::Ended);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::TurnEnded {
                final_reply: None,
                agent,
                turn,
                mode: dudes_types::Mode::Managed,
                provenance: dudes_types::Provenance::Inferred,
                elapsed_ms: None,
                exit: dudes_proto::ExitStatus::UNKNOWN,
                usage: None,
                model_usage: std::collections::BTreeMap::new(),
                last_message: None,
                session: None,
                session_usage: None,
                outcome: Some(dudes_proto::TurnOutcome::Vanished),
            },
        })
        .unwrap();
    drop(writer);
    let host = common::SharedHost::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut restarted = common::start(daemon);
    let second = send(&restarted.handle, &session(), agent, "provenance gated", 1);
    fixture.await_record("note provenance Hold", |record| matches!(record, Record::TaughtNoteTurnDecided { note_lsn, decision: dudes_proto::NoteAdmission::Hold { reason: dudes_proto::HoldReason::InferredTurnEnd }, .. } if *note_lsn == second).then_some(()));
    restarted.handle.doctor().unwrap();
    assert!(host.spawned().is_empty());
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::PredicateResult { .. }))
            .count(),
        1
    );
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    assert_eq!(
        world.agents[&agent].last_turn_end,
        Some(dudes_types::Provenance::Inferred)
    );
    assert!(world.completed_goal_lsn(goal).is_some());
    assert_eq!(world.goals[&goal].turns_on_goal, 2);
    restarted.stop(ExitCause::Ended);
}

#[test]
fn failed_note_only_turns_keep_rapid_failure_protection_without_predicates() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let predicates = Arc::new(common::ScriptedRunner::scripted([common::passes()]));
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(&settings, host.clone(), &host, Arc::clone(&predicates)),
        RunOptions::default(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    fixture.await_record("ordinary Done", |record| matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(()));
    let first = send(&running.handle, &session(), agent, "first failure", 0);
    let second = send(&running.handle, &session(), agent, "second failure", 1);
    let third = send(&running.handle, &session(), agent, "must remain queued", 2);
    for note in [first, second] {
        let turn = issued_note(&fixture, note);
        fixture.await_record("note spawned", |record| {
            matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
                .then_some(())
        });
        let key = owned_spawn_key(&fixture, turn);
        common::end_turn_with_native_payload(
            &fixture,
            &host,
            dudes_supervisor::Input::ChildExited {
                final_reply: None,
                turn,
                elapsed_ms: Some(1),
                exit: dudes_proto::ExitStatus {
                    code: Some(1),
                    signal: None,
                },
                usage: None,
                model_usage: std::collections::BTreeMap::default(),
                outcome: dudes_proto::TurnOutcome::Failed {
                    reason: "owned fast failure".to_owned(),
                },
                last_message: None,
            },
        );
        fixture.await_record("the original failed note end", |record| {
            matches!(record, Record::TurnEnded {
                turn: ended,
                elapsed_ms: Some(1),
                outcome: Some(dudes_proto::TurnOutcome::Failed { .. }),
                ..
            } if *ended == turn)
            .then_some(())
        });
        fixture.await_record("the exact failed note native boundary", |record| {
            matches!(record, Record::InstructionAttemptBoundarySettled {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: ended, .. },
                proof: dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { .. },
                ..
            } if *ended == turn).then_some(())
        });
        host.await_native_acknowledged(&key);
    }
    fixture.await_record("rapid failure Hold", |record| {
        matches!(
            record,
            Record::AgentHeld {
                cause: dudes_proto::AutomaticHold::RepeatedFastFailure { .. },
                ..
            }
        )
        .then_some(())
    });
    running.handle.doctor().unwrap();
    assert_eq!(predicates.runs(), 1);
    assert_eq!(host.spawned().len(), 3);
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::TaughtNoteOnlyTurnIssued { note_lsn, .. } if *note_lsn == third)
    ));
    assert!(fixture.records().iter().any(|record| matches!(record, Record::BreakerBeat { signals, .. } if signals.contains(&dudes_types::BreakerSignal::RapidFailure))));
    running.stop(ExitCause::Ended);
}

#[test]
fn note_lifecycle_keeps_an_unresolved_ordinary_board_predicate_fence() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host,
            Arc::new(common::ScriptedRunner::scripted([common::passes()])),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let completed = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(completed, &[common::true_program()]),
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    fixture.await_record("original completed goal", |record| matches!(record, Record::LoopDecided { agent, decision: LoopDecision::Done, .. } if *agent == completed).then_some(()));
    let waiting = common::hire(&running.handle, fixture.state.path());
    let mut params = common::goal_params(waiting, &[common::false_program()]);
    params.predicate = dudes_proto::Predicate::BoardState {
        card: dudes_types::CardId::NIL,
        status: dudes_types::CardStatus::Done,
    };
    let goal = common::set_goal(&running.handle, params);
    let ordinary = fixture.await_record("waiting ordinary turn", |record| {
        matches!(record, Record::TurnIssued { agent, .. } if *agent == waiting).then(
            || match record {
                Record::TurnIssued { turn, .. } => *turn,
                _ => unreachable!(),
            },
        )
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, ordinary);
    running.handle.doctor().unwrap();
    let note = send(
        &running.handle,
        &session(),
        completed,
        "preserve other goal fence",
        0,
    );
    let turn = issued_note(&fixture, note);
    fixture.await_record("completed note spawned", |record| {
        matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
            .then_some(())
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    running.handle.doctor().unwrap();
    running
        .handle
        .ask(crate::ports::Ask::Resume(waiting))
        .unwrap();
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(
                |record| matches!(record, Record::TurnIssued { agent, .. } if *agent == waiting)
            )
            .count(),
        1,
        "note lifecycle cannot release an ordinary board fence"
    );
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::PredicateResult { goal: judged, .. } if *judged == goal)
    ));
    assert_eq!(host.spawned().len(), 3);
    running.stop(ExitCause::Ended);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the same saved answer must survive two note turns before a later ordinary turn carries it"
)]
fn completed_question_answer_closes_without_trigger_or_delivery() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let predicates = Arc::new(common::ScriptedRunner::scripted([
        crate::predicate::PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: String::new(),
            timed_out: false,
        },
    ]));
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(&settings, host.clone(), &host, predicates),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    fixture.await_record("ordinary Done", |record| {
        matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Done,
                ..
            }
        )
        .then_some(())
    });
    let identity = session();
    let first = send(&running.handle, &identity, agent, "first note", 0);
    let first_turn = issued_note(&fixture, first);
    fixture.await_record("first note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == first_turn).then_some(())
    });
    running.handle.input(dudes_supervisor::Input::Command(
        dudes_supervisor::Command::Ask {
            agent,
            text: "which branch?".into(),
        },
    ));
    running.handle.doctor().unwrap();
    let second = send(&running.handle, &identity, agent, "second note", 1);
    common::end_turn_with_native(&fixture, &running.handle, &host, first_turn);
    running.handle.doctor().unwrap();
    running.handle.input(dudes_supervisor::Input::Command(
        dudes_supervisor::Command::Answer {
            agent,
            text: "saved answer".into(),
            provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
        },
    ));
    running.handle.doctor().unwrap();
    assert_eq!(
        host.spawned().len(),
        2,
        "answer must not request another note-only turn"
    );
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::TaughtNoteOnlyTurnIssued { note_lsn, .. } if *note_lsn == second)));
    assert!(fixture.records().iter().any(
        |record| matches!(record, Record::HumanAnswer { text, .. } if text == "saved answer")
    ));
    let third = send(&running.handle, &identity, agent, "third note", 2);
    let second_turn = issued_note(&fixture, second);
    fixture.await_record("second note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == second_turn).then_some(())
    });
    assert_eq!(
        host.spawned()[2].argv.last().and_then(|part| part.to_str()),
        Some(
            dudes_proto::taught_note_task("second note")
                .unwrap()
                .as_str()
        )
    );
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    assert_eq!(
        world.agents[&agent]
            .pending_answers
            .front()
            .map(|answer| answer.text.as_str()),
        Some("saved answer")
    );
    common::end_turn_with_native(&fixture, &running.handle, &host, second_turn);
    let third_turn = issued_note(&fixture, third);
    fixture.await_record("third note spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == third_turn).then_some(())
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, third_turn);
    running.handle.doctor().unwrap();
    let mut ordinary = common::goal_params(agent, &[common::false_program()]);
    ordinary.text = "future ordinary goal".to_owned();
    let goal = common::set_goal(&running.handle, ordinary);
    let ordinary_turn = fixture.await_record("the future ordinary issue", |record| match record {
        Record::TurnIssued {
            agent: owner,
            goal: issued_goal,
            turn,
            ..
        } if *owner == agent && *issued_goal == goal => Some(*turn),
        _ => None,
    });
    let _ = owned_spawn_key(&fixture, ordinary_turn);
    assert!(
        host.spawned()
            .last()
            .unwrap()
            .argv
            .last()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("saved answer")
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn expired_writer_session_cannot_read_a_durable_chat_receipt() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let ordinary =
        fixture.await_record(
            "the ordinary issue before session expiry",
            |record| match record {
                Record::TurnIssued {
                    agent: owner,
                    goal: issued_goal,
                    turn,
                    ..
                } if *owner == agent && *issued_goal == goal => Some(*turn),
                _ => None,
            },
        );
    let _ = owned_spawn_key(&fixture, ordinary);
    running.handle.doctor().unwrap();
    let (identity, clock) = session_with_clock();
    let original = request(ChatVerb::Send, agent, "private durable receipt", 0);
    assert!(matches!(
        running
            .handle
            .chat(identity.clone(), original.clone())
            .unwrap(),
        ChatOutcome::Queued { .. }
    ));
    let before = fixture.entries().len();
    clock.advance(48 * 60 * 60 * 1_000);
    assert_eq!(
        running.handle.chat(identity, original),
        Err(crate::floor_chat::ChatRefusal::Forbidden)
    );
    assert_eq!(fixture.entries().len(), before);
    running.stop(ExitCause::Ended);
}

#[test]
fn refused_append_permit_releases_only_the_uncommitted_cancellation_claim() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let pause = daemon.browser_launch_pause();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let ordinary = fixture.await_turn();
    let note = send(&running.handle, &session(), agent, "valid reserved note", 0);
    pause.arm();
    common::end_turn(&running.handle, ordinary);
    assert!(pause.wait(Duration::from_secs(5)));
    let clock = Arc::new(PermitClock {
        inner: common::TestClock::new(),
        expire_after_read: AtomicBool::new(false),
    });
    let expired = session_from_clock(Arc::clone(&clock));
    clock.expire_after_read.store(true, Ordering::SeqCst);
    assert_eq!(
        running.handle.chat(
            expired,
            request(ChatVerb::Cancel, agent, &note.0.to_string(), 1)
        ),
        Err(crate::floor_chat::ChatRefusal::Forbidden)
    );
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::BrowserNoteIssuedCancelled { note_lsn, .. } if *note_lsn == note)));
    let outcome = running
        .handle
        .chat(
            session(),
            request(ChatVerb::Cancel, agent, &note.0.to_string(), 2),
        )
        .expect("fresh valid cancellation");
    assert!(matches!(outcome, ChatOutcome::Cancelled { note_lsn, .. } if note_lsn == note));
    pause.release();
    assert!(pause.wait_returned(1, Duration::from_secs(5)));
    running.handle.doctor().unwrap();
    assert_eq!(host.spawned().len(), 1);
    running.stop(ExitCause::Ended);
}

#[test]
fn ordinary_note_reserves_multibyte_task_and_consumes_only_carried_answers() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host,
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let mut goal = common::goal_params(agent, &[common::false_program()]);
    goal.text = "goal context".repeat(8_000);
    common::set_goal(&running.handle, goal);
    let ordinary = fixture.await_turn();
    for (question, answer) in [
        ("first question", "a".repeat(65_536)),
        ("second question", "uncarried answer".to_owned()),
    ] {
        running.handle.input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Ask {
                agent,
                text: question.to_owned(),
            },
        ));
        running.handle.input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Answer {
                agent,
                text: answer,
                provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
            },
        ));
    }
    running.handle.doctor().unwrap();
    let note_text = "é".repeat(2_048);
    let note = send(&running.handle, &session(), agent, &note_text, 0);
    common::end_turn_with_native(&fixture, &running.handle, &host, ordinary);
    let turn = issued_note(&fixture, note);
    fixture.await_record("carrying spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
            .then_some(())
    });
    let task = host.spawned()[1]
        .argv
        .last()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(task.len(), 65_536);
    assert!(task.ends_with(&format!(
        "\n\n[browser note]\n{note_text}\n[end browser note]"
    )));
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    assert_eq!(world.agents[&agent].pending_answers.len(), 1);
    assert_eq!(
        world.agents[&agent].pending_answers.front().unwrap().text,
        "uncarried answer"
    );
    assert_eq!(world.turns[&turn].answers.len(), 1);
    running.stop(ExitCause::Ended);
}

#[test]
fn actual_host_entry_wins_before_any_durable_spawn_receipt() {
    let fixture = common::Fixture::new();
    let host = BoundaryHost::with_native_evidence(fixture.journal_dir());
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host.inner,
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let release = ReleaseHost(host.entry.clone());
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let ordinary = fixture.await_turn();
    let identity = session();
    let note = send(&running.handle, &identity, agent, "actual dispatch wins", 0);
    let _ = owned_spawn_key(&fixture, ordinary);
    host.unknown.store(true, Ordering::SeqCst);
    host.held.store(true, Ordering::SeqCst);
    common::end_turn_with_native(&fixture, &running.handle, &host.inner, ordinary);
    assert!(
        host.entry.wait(),
        "the actual ProcessHost::spawn call must enter"
    );
    let turn = issued_note(&fixture, note);
    assert!(fixture.records().iter().any(|record| matches!(record, Record::BrowserNoteDispatchIntent { turn: intended, .. } if *intended == turn)), "intent must already be durable at host entry");
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
    ));
    let handle = running.handle.clone();
    let cancel = std::thread::spawn(move || {
        handle.chat(
            identity,
            request(ChatVerb::Cancel, agent, &note.0.to_string(), 1),
        )
    });
    release.0.release();
    let outcome = cancel
        .join()
        .expect("owned cancellation caller")
        .expect("cancel response");
    assert!(matches!(
        outcome,
        ChatOutcome::Refused {
            reason: ChatReason::DeliveryStarted,
            ..
        }
    ));
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::BrowserNoteIssuedCancelled { turn: cancelled, .. } | Record::BrowserNoteReissueCancelled { turn: cancelled, .. } if *cancelled == turn)));
    running.stop(ExitCause::Ended);
    assert_acknowledged_prefixes(&fixture);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one test follows the same historical attempt across shutdown, held reissue, cancellation, and accounting"
)]
fn historical_intent_stays_ambiguous_when_new_preparation_never_dispatches() {
    let fixture = common::Fixture::new();
    let host = BoundaryHost::with_native_evidence(fixture.journal_dir());
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host.inner,
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let ordinary = fixture.await_turn();
    running.handle.input(dudes_supervisor::Input::Command(
        dudes_supervisor::Command::Ask {
            agent,
            text: "which branch?".to_owned(),
        },
    ));
    running.handle.input(dudes_supervisor::Input::Command(
        dudes_supervisor::Command::Answer {
            agent,
            text: "reserved answer".to_owned(),
            provenance: Some(dudes_proto::PeerProvenance::NoRecordedAgent),
        },
    ));
    running.handle.doctor().unwrap();
    let note = send(
        &running.handle,
        &session(),
        agent,
        "uncertain old attempt",
        0,
    );
    let _ = owned_spawn_key(&fixture, ordinary);
    host.unknown.store(true, Ordering::SeqCst);
    common::end_turn_with_native(&fixture, &running.handle, &host.inner, ordinary);
    let turn = issued_note(&fixture, note);
    fixture.await_record("first dispatch intent", |record| matches!(record, Record::BrowserNoteDispatchIntent { turn: intended, .. } if *intended == turn).then_some(()));
    running.handle.doctor().unwrap();
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)
    ));
    running.stop(ExitCause::Ended);
    let mut before = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        before.apply(lsn, &entry);
    }
    let task = before.turns[&turn].task.clone();
    let answers = before.agents[&agent].pending_answers.clone();
    assert_eq!(
        answers.front().map(|answer| answer.text.as_str()),
        Some("reserved answer")
    );
    let pause = crate::BrowserLaunchPause::default();
    pause.arm();
    let restarted_host = common::SharedHost::new();
    let settings = fixture.settings();
    let daemon = crate::boot::boot_with_preparation_pause(
        &settings,
        with_owned_program(
            &settings,
            restarted_host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
        pause.clone(),
    )
    .expect("restart");
    let mut restarted = common::start(daemon);
    restarted.handle.input(dudes_supervisor::Input::Command(
        dudes_supervisor::Command::Resume {
            agent,
            browser: None,
        },
    ));
    restarted.handle.doctor().unwrap();
    assert!(!pause.wait(Duration::ZERO));
    assert!(restarted_host.spawned().is_empty());
    consent_to_chat_reissue(&mut |ask| restarted.handle.ask(ask), &fixture, agent, turn);
    assert!(
        pause.wait(Duration::from_secs(5)),
        "new attempt is held preparation only after explicit consent"
    );
    let outcome = restarted
        .handle
        .chat(
            session(),
            request(ChatVerb::Cancel, agent, &note.0.to_string(), 1),
        )
        .expect("cancel future replay");
    assert!(matches!(outcome, ChatOutcome::ReissueCancelled { note_lsn, .. } if note_lsn == note));
    pause.release();
    assert!(pause.wait_returned(1, Duration::from_secs(5)));
    restarted.handle.doctor().unwrap();
    assert!(restarted_host.spawned().is_empty());
    assert!(
        restarted
            .handle
            .closing_call(crate::closing::Request::Announce)
            .is_err(),
        "unresolved usage must retain closing fence"
    );
    restarted.stop(ExitCause::Ended);
    let mut after = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        after.apply(lsn, &entry);
    }
    assert_eq!(after.agents[&agent].open_turn, Some(turn));
    assert_eq!(after.agents[&agent].pending_answers, answers);
    assert_eq!(
        after.agents[&agent].last_turn_end,
        before.agents[&agent].last_turn_end
    );
    assert_eq!(
        after.goals[&goal].turns_on_goal,
        before.goals[&goal].turns_on_goal
    );
    assert_eq!(after.turns[&turn].task, task);
    assert_eq!(
        after.browser_issue(turn).unwrap().dispatch_intents,
        before.browser_issue(turn).unwrap().dispatch_intents
    );
    assert!(
        !fixture.records().iter().any(
            |record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn)
        )
    );
    assert_acknowledged_prefixes(&fixture);
}

#[derive(Clone, Default)]
struct RecoveryResponseBytes(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for RecoveryResponseBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn local_recovery_call(
    execute: &mut impl FnMut(
        crate::ports::Ask,
    ) -> Result<crate::ports::Answered, dudes_proto::rpc::RpcError>,
    method: &'static str,
    params: serde_json::Value,
) -> serde_json::Value {
    let (jobs, received) = std::sync::mpsc::channel();
    let mut dispatcher = dudes_rpc::Dispatcher::new(dudes_rpc::PeerClassifier::new(
        dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap(),
        Arc::new(crate::classify::WorldTokens::new()),
    ));
    dudes_rpc::methods::register_recovery(&mut dispatcher, &Arc::new(Handle::new(jobs)));
    let response = RecoveryResponseBytes::default();
    let captured = response.clone();
    let serving = std::thread::spawn(move || {
        let frame = format!(
            "{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params,"cred":"C".repeat(43)})
        );
        dudes_rpc::server::serve_stream(
            std::io::Cursor::new(frame.into_bytes()),
            &Arc::new(dudes_rpc::Outbound::new(captured)),
            dudes_rpc::PeerIdentity {
                uid: Some(501),
                gid: None,
                pid: dudes_rpc::PeerPid::Reported {
                    pid: 4242,
                    pidversion: None,
                },
            },
            dudes_rpc::PeerProvenance::NoRecordedAgent,
            &dispatcher,
        )
        .unwrap();
    });
    let crate::ports::Job::Ask { ask, reply } = received
        .recv_timeout(Duration::from_secs(5))
        .expect("the authenticated route forwards one typed recovery command")
    else {
        panic!("one recovery ask");
    };
    reply.send(execute(ask)).unwrap();
    serving.join().unwrap();
    let response: dudes_proto::rpc::Response =
        serde_json::from_slice(&response.0.lock().unwrap()).unwrap();
    assert!(response.error.is_none(), "{response:?}");
    response.result.expect("the actual recovery result")
}

fn consent_to_chat_reissue(
    execute: &mut impl FnMut(
        crate::ports::Ask,
    ) -> Result<crate::ports::Answered, dudes_proto::rpc::RpcError>,
    fixture: &common::Fixture,
    agent: AgentId,
    turn: TurnId,
) {
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult,
        RecoverySubmitParams, RecoverySubmitResult,
    };

    let original_issue = fixture.entries().into_iter().find_map(|(lsn, entry)| {
        matches!(entry.record, Record::TurnIssuedWithBrowserNote { turn: issued, .. } if issued == turn)
            .then_some(lsn)
    }).expect("the original goal-bearing browser issue");
    let inspected: RecoveryInspectResult = serde_json::from_value(local_recovery_call(
        execute,
        "recovery.inspect",
        serde_json::to_value(RecoveryInspectParams { agent }).unwrap(),
    ))
    .unwrap();
    let RecoveryInspectResult::Available {
        expected,
        reason,
        choices,
        ..
    } = inspected
    else {
        panic!("the structurally eligible owed issue offers recovery: {inspected:?}");
    };
    assert_eq!(expected.work.agent, agent);
    assert_eq!(expected.work.turn, turn);
    assert_eq!(expected.issue_lsn, original_issue);
    assert_eq!(reason, dudes_proto::recovery::RecoveryReason::MissingSpawn);
    assert_eq!(choices, RecoveryAllowedChoices::RepeatIssuedFresh);
    let episode = expected.episode.expect("restart durably required recovery");
    assert!(fixture.entries().iter().any(|(lsn, entry)| {
        matches!(&entry.record, Record::RecoveryRequired(required)
            if *lsn == episode.origin && required.binding.work == episode.work)
    }));
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::RecoveryAuthorized(_)))
    );
    let request_id = dudes_proto::record::Hex32::new([193; 32]);
    let submitted: RecoverySubmitResult = serde_json::from_value(local_recovery_call(
        execute,
        "recovery.submit",
        serde_json::to_value(RecoverySubmitParams {
            request_id,
            expected: *expected,
            choice: RecoveryChoice::RepeatIssuedFresh,
        })
        .unwrap(),
    ))
    .unwrap();
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("the explicit owed-work consent is accepted: {submitted:?}");
    };
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(receipt.choice, RecoveryChoice::RepeatIssuedFresh);
    assert_eq!(receipt.claim.episode, episode);
    let authorized = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::RecoveryAuthorized(value) if value.request_id == request_id => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(authorized.len(), 1);
    assert_eq!(authorized[0].episode, episode);
    assert_eq!(
        authorized[0].attribution,
        dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 4242,
            uid: Some(501),
        }
    );
    assert!(
        matches!(&authorized[0].plan, dudes_proto::recovery::RecoveryPlan::RepeatIssued { issue, .. }
        if issue.lsn == original_issue && issue.issued.work == episode.work)
    );
}

fn consent_to_direct_reissue<H: dudes_supervisor::ProcessHost>(
    daemon: &mut PreparationDaemon<H>,
    fixture: &common::Fixture,
    agent: AgentId,
    turn: TurnId,
) {
    let before = daemon
        .sup
        .world()
        .instructions
        .current_for_turn(turn)
        .map(|(lsn, _)| lsn);
    daemon
        .pump_runtime_input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Resume {
                agent,
                browser: None,
            },
        ))
        .unwrap();
    assert_eq!(
        daemon
            .sup
            .world()
            .instructions
            .current_for_turn(turn)
            .map(|(lsn, _)| lsn),
        before,
        "ordinary Resume does not prepare the old issue"
    );
    assert!(daemon.sup.world().turns[&turn].is_issued_not_spawned());
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::TurnSpawned { turn: spawned, .. } if *spawned == turn)));
    consent_to_chat_reissue(&mut |ask| daemon.ask(ask), fixture, agent, turn);
}

struct HistoricalReplay {
    fixture: common::Fixture,
    agent: AgentId,
    goal: dudes_types::GoalId,
    turn: TurnId,
    note: Lsn,
    before: dudes_supervisor::World,
    spend: String,
}

impl HistoricalReplay {
    fn new() -> Self {
        Self::recorded(false)
    }

    fn recorded(halted: bool) -> Self {
        let fixture = common::Fixture::new();
        let host = BoundaryHost::with_native_evidence(fixture.journal_dir());
        let settings = fixture.settings();
        let daemon = boot_with_options(
            &settings,
            with_native_program(
                &settings,
                host.clone(),
                &host.inner,
                Arc::new(common::ScriptedRunner::default()),
            ),
            RunOptions::default(),
        )
        .unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let goal = common::set_goal(
            &running.handle,
            common::goal_params(agent, &[common::false_program()]),
        );
        let ordinary = fixture.await_turn();
        running
            .handle
            .ask(crate::ports::Ask::Ask {
                agent,
                text: "keep this answer reserved".to_owned(),
            })
            .unwrap();
        running
            .handle
            .ask(crate::ports::Ask::Answer {
                agent,
                text: "possibly carried before restart".to_owned(),
                provenance: dudes_proto::PeerProvenance::NoRecordedAgent,
            })
            .unwrap();
        let note = send(
            &running.handle,
            &session(),
            agent,
            "historical unresolved note",
            0,
        );
        let _ = owned_spawn_key(&fixture, ordinary);
        host.unknown.store(true, Ordering::SeqCst);
        common::end_turn_with_native(&fixture, &running.handle, &host.inner, ordinary);
        let turn = issued_note(&fixture, note);
        fixture.await_record("historical intent", |record| matches!(record, Record::BrowserNoteDispatchIntent { turn: intended, .. } if *intended == turn).then_some(()));
        running.handle.doctor().unwrap();
        if halted {
            running.handle.ask(crate::ports::Ask::Halt(agent)).unwrap();
        }
        running.stop(ExitCause::Ended);
        let mut before = dudes_supervisor::World::default();
        for (lsn, entry) in fixture.entries() {
            before.apply(lsn, &entry);
        }
        let projection = dudes_journal::Projections::open(
            &fixture.state.path().join(crate::boot::PROJECTION_FILE),
        )
        .unwrap();
        let day = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
        let spend = format!("{:?}", projection.spend_views(agent, goal, day).unwrap());
        Self {
            fixture,
            agent,
            goal,
            turn,
            note,
            before,
            spend,
        }
    }
}

fn assert_historical_replay_is_unsettled(history: &HistoricalReplay, failure: &str) {
    let mut after = dudes_supervisor::World::default();
    for (lsn, entry) in history.fixture.entries() {
        after.apply(lsn, &entry);
    }
    assert_eq!(
        after.agents[&history.agent].open_turn,
        Some(history.turn),
        "{failure} failure cannot settle an old dispatch"
    );
    assert_eq!(
        after.agents[&history.agent].pending_answers,
        history.before.agents[&history.agent].pending_answers
    );
    assert_eq!(
        after.agents[&history.agent].last_turn_end,
        history.before.agents[&history.agent].last_turn_end
    );
    assert_eq!(
        after.goals[&history.goal].turns_on_goal,
        history.before.goals[&history.goal].turns_on_goal
    );
    assert!(
        !history.fixture.records().iter().any(
            |record| matches!(record, Record::TurnEnded { turn, .. } if *turn == history.turn)
        )
    );
}

fn assert_failed_replay_preserves_history(failure: &str) {
    let history = HistoricalReplay::new();
    let host = common::SharedHost::with(dudes_supervisor::testing::FakeHost {
        fail_spawn: (failure == "host").then_some(std::io::ErrorKind::NotFound),
        ..dudes_supervisor::testing::FakeHost::wall_clock_anchored()
    });
    let settings = history.fixture.settings();
    let predicates = Arc::new(common::ScriptedRunner::default());
    let mut seams = with_owned_program(&settings, host.clone(), predicates);
    seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(ClaudeConfig {
        command: if failure == "resolution" {
            history
                .fixture
                .runtime
                .path()
                .join("missing-owned-program")
                .into_os_string()
        } else {
            std::env::current_exe().unwrap().into_os_string()
        },
        setting_sources: (failure == "adapter").then_some(vec![
            dudes_providers::claude::config::SettingSource::Project,
        ]),
        ..ClaudeConfig::default()
    })) as Box<dyn Adapter>]);
    let pause = crate::BrowserLaunchPause::default();
    if failure != "adapter" {
        pause.arm();
    }
    let daemon = crate::boot::boot_with_preparation_pause(
        &settings,
        seams,
        RunOptions::default(),
        pause.clone(),
    )
    .unwrap();
    let mut restarted = common::start(daemon);
    if failure != "adapter" {
        restarted.handle.input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Resume {
                agent: history.agent,
                browser: None,
            },
        ));
        restarted.handle.doctor().unwrap();
        assert!(!pause.wait(Duration::ZERO));
        assert!(host.spawned().is_empty());
        consent_to_chat_reissue(
            &mut |ask| restarted.handle.ask(ask),
            &history.fixture,
            history.agent,
            history.turn,
        );
        assert!(pause.wait(Duration::from_secs(5)));
        pause.release();
        assert!(pause.wait_returned(1, Duration::from_secs(5)));
    }
    restarted.handle.doctor().unwrap();
    assert_historical_replay_is_unsettled(&history, failure);
    let outcome = restarted
        .handle
        .chat(
            session(),
            request(
                ChatVerb::Cancel,
                history.agent,
                &history.note.0.to_string(),
                1,
            ),
        )
        .unwrap();
    assert!(matches!(outcome, ChatOutcome::ReissueCancelled { .. }));
    assert!(host.spawned().is_empty());
    restarted.stop(ExitCause::Ended);
    let projection_path = history
        .fixture
        .state
        .path()
        .join(crate::boot::PROJECTION_FILE);
    let projection = dudes_journal::Projections::open(&projection_path).unwrap();
    let day = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
    assert_eq!(
        format!(
            "{:?}",
            projection
                .spend_views(history.agent, history.goal, day)
                .unwrap()
        ),
        history.spend
    );
}

#[test]
fn historical_failed_resolution_keeps_dispatch_uncertainty() {
    assert_failed_replay_preserves_history("resolution");
}

#[test]
fn historical_failed_adapter_keeps_dispatch_uncertainty() {
    assert_failed_replay_preserves_history("adapter");
}

#[test]
fn historical_failed_host_keeps_dispatch_uncertainty() {
    assert_failed_replay_preserves_history("host");
}

type PreparationDaemon<H = common::SharedHost> =
    super::Daemon<H, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn drive_preparation_pause<H: dudes_supervisor::ProcessHost>(
    daemon: &mut PreparationDaemon<H>,
    pause: &crate::BrowserLaunchPause,
) -> Option<dudes_supervisor::Input> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !pause.wait(Duration::ZERO) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match daemon
            .jobs
            .recv_timeout(remaining.min(Duration::from_millis(10)))
        {
            Ok(crate::ports::Job::Input(input)) => {
                if matches!(&*input, dudes_supervisor::Input::BrowserLaunchReady { .. }) {
                    return Some(*input);
                }
                daemon.pump_runtime_input(*input).unwrap();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the actual preparation job queue ended")
            }
            Ok(_) => panic!("unexpected non-input preparation job"),
        }
    }
    None
}

fn actual_preparation<H: dudes_supervisor::ProcessHost>(
    daemon: &mut PreparationDaemon<H>,
) -> dudes_supervisor::Input {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "the actual preparation did not arrive"
        );
        match daemon
            .jobs
            .recv_timeout(remaining)
            .expect("actual preparation job")
        {
            crate::ports::Job::Input(input) => {
                if matches!(&*input, dudes_supervisor::Input::BrowserLaunchReady { .. }) {
                    return *input;
                }
                daemon.pump_runtime_input(*input).unwrap();
            }
            _ => panic!("unexpected non-input preparation job"),
        }
    }
}

fn pump_proposed_dispatch(
    daemon: &mut PreparationDaemon<BoundaryHost>,
    input: dudes_supervisor::Input,
) -> (
    dudes_supervisor::Input,
    dudes_supervisor::input::PersistToken,
    dudes_supervisor::Input,
) {
    let dudes_supervisor::Input::BrowserLaunchReady {
        turn,
        generation,
        prepared,
    } = input
    else {
        panic!("expected the actual preparation callback")
    };
    let duplicate = dudes_supervisor::Input::BrowserLaunchReady {
        turn,
        generation,
        prepared: prepared.clone(),
    };
    daemon.ctx.resolved_browser_turn = Some((turn, generation));
    daemon
        .sup
        .handle(
            dudes_supervisor::Input::BrowserLaunchReady {
                turn,
                generation,
                prepared,
            },
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    let effects = daemon.sup.take_effects();
    let [effect @ dudes_supervisor::Effect::Persist { token, entry }] = effects.as_slice() else {
        panic!("the actual prepared retry requests its one dispatch intent");
    };
    assert!(
        matches!(entry.record, Record::BrowserNoteDispatchIntent { turn: issued, .. } if issued == turn)
    );
    let proposed_token = *token;
    let mut queue = std::collections::VecDeque::new();
    daemon
        .ctx
        .dispatch_browser_intent(effect, &mut queue, &mut daemon.sup, &mut daemon.closing)
        .unwrap();
    daemon.ctx.resolved_browser_turn = None;
    let held_spawn = queue.pop_front().expect("the actual host reply");
    let dudes_supervisor::Input::Spawned { turn: spawned, .. } = &held_spawn else {
        panic!("the actual host reply is withheld before its journal acknowledgement");
    };
    assert_eq!(*spawned, turn);
    assert!(queue.is_empty());
    assert!(daemon.sup.world().turns[&turn].is_issued_not_spawned());
    assert_eq!(daemon.ctx.pending_native_launches.len(), 1);
    (duplicate, proposed_token, held_spawn)
}

fn assert_uncertain_chat_history(
    history: &HistoricalReplay,
    daemon: &PreparationDaemon<BoundaryHost>,
    host: &BoundaryHost,
) {
    let after = daemon.sup.world();
    let before = &history.before;
    let issue = after.browser_issue(history.turn).unwrap();
    let prior_issue = before.browser_issue(history.turn).unwrap();
    assert_eq!(issue.issue_lsn, prior_issue.issue_lsn);
    assert_eq!(issue.note_lsn, history.note);
    assert_eq!(issue.goal, Some(history.goal));
    assert!(daemon.sup.browser_delivery_uncertain(history.turn));
    assert_eq!(
        issue.dispatch_intents, prior_issue.dispatch_intents,
        "preparation cannot append an intent past the unresolved native boundary"
    );
    assert!(issue.spawn_lsn.is_none());
    assert!(issue.end_lsn.is_none());
    assert_eq!(
        after.turns[&history.turn].task,
        before.turns[&history.turn].task
    );
    assert_eq!(
        after.turns[&history.turn].answers,
        before.turns[&history.turn].answers
    );
    assert_eq!(after.agents[&history.agent].open_turn, Some(history.turn));
    assert_eq!(
        after.agents[&history.agent].pending_answers,
        before.agents[&history.agent].pending_answers
    );
    assert_eq!(
        after.agents[&history.agent].last_turn_end,
        before.agents[&history.agent].last_turn_end
    );
    assert_eq!(
        after.goals[&history.goal].turns_on_goal,
        before.goals[&history.goal].turns_on_goal
    );
    assert_eq!(
        after
            .instructions
            .unresolved_launch_attempts(history.agent)
            .collect::<Vec<_>>(),
        before
            .instructions
            .unresolved_launch_attempts(history.agent)
            .collect::<Vec<_>>()
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.inner.spawned().len(), 1);
    assert!(
        host.not_launched.load(Ordering::SeqCst),
        "even a refused additional host call would consume this control"
    );
    assert_eq!(daemon.ctx.pending_native_launches.len(), 1);
    assert!(
        matches!(daemon.ctx.admit_prior_attempt(&daemon.sup, history.agent, Some(history.turn)),
        Err(dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { agent }) if agent == history.agent)
    );
    assert!(
        !history.fixture.records().iter().any(|record| match record {
            Record::TurnSpawned { turn, .. } | Record::TurnEnded { turn, .. } => {
                *turn == history.turn
            }
            Record::InstructionAttemptBoundarySettled {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. },
                ..
            } => *turn == history.turn,
            _ => false,
        })
    );
    let day = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
    assert_eq!(
        format!(
            "{:?}",
            daemon
                .ctx
                .projections
                .spend_views(history.agent, history.goal, day)
                .unwrap()
        ),
        history.spend
    );
}

fn freeze_pending_chat_prefix(
    history: &mut HistoricalReplay,
    daemon: &PreparationDaemon<BoundaryHost>,
    host: &BoundaryHost,
    original: &[Record],
) {
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.inner.spawned().len(), 1);
    assert_eq!(
        daemon.sup.world().turns[&history.turn].task,
        history.before.turns[&history.turn].task
    );
    assert_eq!(
        daemon
            .sup
            .world()
            .browser_issue(history.turn)
            .unwrap()
            .issue_lsn,
        history
            .before
            .browser_issue(history.turn)
            .unwrap()
            .issue_lsn
    );
    assert!(history.fixture.records().starts_with(original));
    let mut pending = dudes_supervisor::World::default();
    for (lsn, entry) in history.fixture.entries() {
        pending.apply(lsn, &entry);
    }
    history.before = pending;
    host.not_launched.store(true, Ordering::SeqCst);
}

fn historical_dispatch_ack(
    history: &HistoricalReplay,
    token: dudes_supervisor::input::PersistToken,
    lsn: Lsn,
) -> dudes_supervisor::Input {
    let entry = history
        .fixture
        .entries()
        .into_iter()
        .find(|(at, _)| *at == lsn)
        .expect("the fixture recorded this historical dispatch position")
        .1;
    dudes_supervisor::Input::Persisted {
        token,
        append: dudes_supervisor::testing::recorded_append(lsn, entry),
    }
}

#[test]
fn actual_late_preparation_and_proposed_ack_keep_uncertainty_through_resume() {
    let mut history = HistoricalReplay::new();
    let original = history.fixture.records();
    let prior = *history
        .before
        .browser_issue(history.turn)
        .unwrap()
        .dispatch_intents
        .last()
        .unwrap();
    let host = BoundaryHost::new();
    let pause = crate::BrowserLaunchPause::default();
    pause.arm();
    let settings = history.fixture.settings();
    let mut daemon = crate::boot::boot_with_preparation_pause(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
        pause.clone(),
    )
    .unwrap();
    assert!(!pause.wait(Duration::ZERO));
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    consent_to_direct_reissue(&mut daemon, &history.fixture, history.agent, history.turn);
    let parked = drive_preparation_pause(&mut daemon, &pause);
    assert!(pause.wait(Duration::from_secs(5)));
    pause.release();
    assert!(pause.wait_returned(1, Duration::from_secs(5)));
    let prepared = parked.unwrap_or_else(|| actual_preparation(&mut daemon));
    let (duplicate, proposed_token, held_spawn) = pump_proposed_dispatch(&mut daemon, prepared);
    let dudes_supervisor::Input::BrowserLaunchReady { turn, .. } = &duplicate else {
        unreachable!()
    };
    let turn = *turn;
    assert_eq!(turn, history.turn);
    assert!(
        matches!(&held_spawn, dudes_supervisor::Input::Spawned { turn: returned, key, .. }
        if *returned == turn && host.inner.pids() == [key.pid])
    );
    freeze_pending_chat_prefix(&mut history, &daemon, &host, &original);
    let frozen = history.fixture.records();
    assert_uncertain_chat_history(&history, &daemon, &host);
    let refusal = dudes_supervisor::machine::BrowserNoteCancelDisposition::Refused {
        reason: ChatReason::DeliveryStarted,
    };
    assert_eq!(
        daemon
            .sup
            .browser_note_cancel_disposition(history.agent, history.note),
        refusal
    );
    daemon.pump_runtime_input(duplicate).unwrap();
    assert_uncertain_chat_history(&history, &daemon, &host);
    daemon
        .pump_runtime_input(historical_dispatch_ack(&history, proposed_token, prior))
        .unwrap();
    assert_eq!(
        daemon
            .sup
            .browser_note_cancel_disposition(history.agent, history.note),
        refusal,
        "late callback must retain the earlier unresolved intent proof"
    );
    assert_uncertain_chat_history(&history, &daemon, &host);
    daemon
        .pump_runtime_input(dudes_supervisor::Input::Command(
            dudes_supervisor::Command::Resume {
                agent: history.agent,
                browser: None,
            },
        ))
        .unwrap();
    assert_uncertain_chat_history(&history, &daemon, &host);
    assert_eq!(
        daemon
            .sup
            .browser_note_cancel_disposition(history.agent, history.note),
        refusal,
        "Resume cannot certify the earlier launch's native absence"
    );
    assert_eq!(
        history
            .fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteDispatchIntent { .. }))
            .count(),
        frozen
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteDispatchIntent { .. }))
            .count()
    );
}

fn start_finite_nonlaunch_chat(fixture: &common::Fixture, host: &BoundaryHost) -> common::Running {
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_native_program(
            &settings,
            host.clone(),
            &host.inner,
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    common::start(daemon)
}

#[test]
fn finite_nonlaunch_preserves_confirmed_session_and_holds_follow_up() {
    let fixture = common::Fixture::new();
    let host = BoundaryHost::with_native_evidence(fixture.journal_dir());
    let mut running = start_finite_nonlaunch_chat(&fixture, &host);
    let agent = common::hire(&running.handle, fixture.state.path());
    let browser = session();
    let first = send(&running.handle, &browser, agent, "establish context", 0);
    let first_turn = issued_note(&fixture, first);
    fixture.await_record("first finite spawn", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == first_turn).then_some(())
    });
    let confirmed = fixture.await_record("first finite session", |record| match record {
        Record::ConversationTurnIssued {
            turn,
            session: Some(session),
            ..
        } if *turn == first_turn => Some(session.clone()),
        _ => None,
    });
    running
        .handle
        .input(dudes_supervisor::Input::SessionLearned {
            turn: first_turn,
            session: confirmed.clone(),
        });
    common::end_turn_with_native(&fixture, &running.handle, &host.inner, first_turn);
    running.handle.doctor().unwrap();
    host.not_launched.store(true, Ordering::SeqCst);
    let second = send(&running.handle, &browser, agent, "failed launch", 1);
    let second_turn = issued_note(&fixture, second);
    fixture.await_record("finite launch failure Hold", |record| {
        matches!(record, Record::AgentHeld { agent: owner, cause: dudes_proto::AutomaticHold::LaunchFailed { .. }, .. } if *owner == agent).then_some(())
    });
    let held = send(&running.handle, &browser, agent, "retained context", 2);
    running.handle.doctor().unwrap();
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(record,
        Record::ConversationTurnIssued { turn, session: Some(session), .. } if *turn == second_turn && session == &confirmed)));
    assert!(records.iter().any(|record| matches!(record,
        Record::TurnEnded { turn, outcome: Some(dudes_proto::TurnOutcome::NotLaunched { .. }), .. } if *turn == second_turn)));
    assert!(!records.iter().any(|record| matches!(record,
        Record::TurnSpawned { turn, .. } if *turn == second_turn)));
    assert!(!records.iter().any(|record| matches!(record,
        Record::ConversationTurnIssued { note_lsn, .. } if *note_lsn == held)));
    let replay = tempfile::tempdir().unwrap();
    let mut projection =
        dudes_journal::Projections::open(&replay.path().join("state.redb")).unwrap();
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
        projection.apply(lsn, 0, &entry).unwrap();
    }
    assert_eq!(world.agents[&agent].session.as_ref(), Some(&confirmed));
    assert_eq!(world.conversation_context_loss(agent), None);
    let projected = projection.agent(agent).unwrap().unwrap();
    assert_eq!(projected.session.as_ref(), Some(&confirmed));
    assert_eq!(projected.conversation_context_loss, None);
    assert!(matches!(
        world.agents[&agent].automatic_hold,
        Some(dudes_proto::AutomaticHold::LaunchFailed { .. })
    ));
    running
        .handle
        .ask(crate::ports::Ask::Resume(agent))
        .unwrap();
    let resumed = issued_note(&fixture, held);
    fixture.await_record("finite retry keeps confirmed context", |record| {
        matches!(record, Record::ConversationTurnIssued { turn, session: Some(session), .. } if *turn == resumed && session == &confirmed).then_some(())
    });
    fixture.await_record("finite retry spawned", |record| {
        matches!(record, Record::TurnSpawned { turn, .. } if *turn == resumed).then_some(())
    });
    assert_eq!(host.calls.load(Ordering::SeqCst), 3);
    running
        .handle
        .input(dudes_supervisor::Input::SessionLearned {
            turn: resumed,
            session: confirmed.clone(),
        });
    common::end_turn_with_native(&fixture, &running.handle, &host.inner, resumed);
    running.stop(ExitCause::Ended);
    let projection =
        dudes_journal::Projections::open(&fixture.state.path().join(crate::boot::PROJECTION_FILE))
            .unwrap();
    let row = projection.agent(agent).unwrap().unwrap();
    assert_eq!(row.session, Some(confirmed));
    assert_eq!(row.conversation_context_loss, None);
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::GoalSet { .. } | Record::ConversationContextResetQueued { .. }
    )));
}

#[test]
fn a_goal_free_open_turn_blocks_replacement_after_a_goal_is_set() {
    let fixture = common::Fixture::new();
    let host = BoundaryHost::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let note = send(&running.handle, &session(), agent, "open conversation", 0);
    let turn = issued_note(&fixture, note);
    fixture.await_record("conversation child", |record| {
        matches!(record, Record::TurnSpawned { turn: current, .. } if *current == turn)
            .then_some(())
    });
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let before = fixture.records();
    let response = running
        .handle
        .ask(crate::ports::Ask::ReplaceGoal(Box::new(
            dudes_proto::rpc::goal::GoalReplaceParams {
                request_id: dudes_proto::record::Hex32::new([71; 32]),
                agent,
                expected_goal: goal,
                text: "replacement after conversation".into(),
                predicate: dudes_proto::Predicate::Command {
                    argv: vec!["/usr/bin/true".into()],
                    timeout_s: 5,
                },
                check: dudes_proto::rpc::goal::CompletionCheckDeclaration {
                    id: "conversation-check".into(),
                    label: "Conversation check".into(),
                },
                may_already_pass: false,
            },
        )))
        .unwrap();
    assert!(matches!(
        response,
        crate::ports::Answered::GoalReplaced(dudes_proto::rpc::goal::GoalReplaceResult::Refused {
            reason: dudes_proto::rpc::goal::GoalReplaceRefusal::TurnOpen,
            ..
        })
    ));
    assert_eq!(fixture.records(), before);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        before
            .iter()
            .filter(|entry| matches!(entry, Record::ConversationTurnIssued { .. }))
            .count(),
        1
    );
    assert!(
        !before
            .iter()
            .any(|entry| matches!(entry, Record::TurnIssued { .. } | Record::TurnEnded { .. }))
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn first_browser_attempt_known_nonlaunch_keeps_existing_failure_accounting() {
    for failure in ["resolution", "host"] {
        let fixture = common::Fixture::new();
        let program = fixture
            .runtime
            .path()
            .join(format!("owned-prepare{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&program, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let host = BoundaryHost::with_native_evidence(fixture.journal_dir());
        let predicates = Arc::new(common::ScriptedRunner::scripted([common::passes()]));
        let settings = fixture.settings();
        let mut seams = with_native_program(
            &settings,
            host.clone(),
            &host.inner,
            Arc::clone(&predicates),
        );
        seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(ClaudeConfig {
            command: program.clone().into_os_string(),
            ..ClaudeConfig::default()
        })) as Box<dyn Adapter>]);
        let daemon = boot_with_options(&settings, seams, RunOptions::default()).unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let goal = common::set_goal(
            &running.handle,
            common::goal_params(agent, &[common::true_program()]),
        );
        common::end_turn_with_native(&fixture, &running.handle, &host.inner, fixture.await_turn());
        fixture.await_record("original Done", |record| matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(()));
        if failure == "resolution" {
            std::fs::remove_file(program).unwrap();
        } else {
            host.not_launched.store(true, Ordering::SeqCst);
        }
        let note = send(
            &running.handle,
            &session(),
            agent,
            "positively unused first attempt",
            0,
        );
        let turn = issued_note(&fixture, note);
        fixture.await_record("observed note nonlaunch", |record| matches!(record, Record::TurnEnded { turn: ended, provenance: dudes_types::Provenance::Observed, outcome: Some(dudes_proto::TurnOutcome::NotLaunched { .. }), .. } if *ended == turn).then_some(()));
        running.stop(ExitCause::Ended);
        assert_eq!(predicates.runs(), 1);
        let mut world = dudes_supervisor::World::default();
        for (lsn, entry) in fixture.entries() {
            world.apply(lsn, &entry);
        }
        assert!(
            matches!(world.browser_note(note).unwrap().state, dudes_supervisor::world::BrowserNoteState::Failed { turn: failed } if failed == turn)
        );
        assert!(world.agents[&agent].open_turn.is_none());
        assert_eq!(world.goals[&goal].turns_on_goal, 2);
        assert!(world.completed_goal_lsn(goal).is_some());
        let projection = dudes_journal::Projections::open(
            &fixture.state.path().join(crate::boot::PROJECTION_FILE),
        )
        .unwrap();
        let day = dudes_types::Timestamp::from_unix_millis(1_787_394_600_000).utc_day();
        assert_eq!(
            projection
                .spend_views(agent, goal, day)
                .unwrap()
                .goal_unknown_ends,
            2,
            "existing usage=None failure accounting remains conservative"
        );
        drop(projection);
        assert_mcp_edit_after_known_browser_end(&fixture, agent);
    }
}

fn assert_mcp_edit_after_known_browser_end(fixture: &common::Fixture, agent: AgentId) {
    let definitions = dudes_providers::mcp::parse_servers(
        toml::from_str("[a]\nurl='http://127.0.0.1:4501/mcp'\n").unwrap(),
    )
    .unwrap();
    let host = common::SharedHost::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default().with_mcp_servers(definitions),
    )
    .unwrap();
    let mut restarted = common::start(daemon);
    assert!(
        matches!(restarted.handle.ask(crate::ports::Ask::Mcp(dudes_proto::rpc::agent::McpParams {
        agent,
        mcp_servers: Some(vec![dudes_types::McpServerName::parse("a").unwrap()]),
    })).unwrap(), crate::ports::Answered::Mcp(result) if result.changed)
    );
    assert!(
        host.spawned().is_empty(),
        "an edit after a recorded nonlaunch does not launch work"
    );
    restarted.stop(ExitCause::Ended);
}

fn chat_capacity_history(fixture: &common::Fixture, agent: AgentId) {
    let mut projection =
        dudes_journal::Projections::open(&fixture.state.path().join(crate::boot::PROJECTION_FILE))
            .unwrap();
    let snapshot = crate::floor::snapshot(
        &mut projection,
        dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
    )
    .unwrap();
    assert_eq!(snapshot.agents[0].id, agent);
    let mut card = dudes_proto::CardView {
        id: dudes_types::CardId::from_parts(1, 20),
        title: String::new(),
        department: "infra".to_owned(),
        status: dudes_types::CardStatus::Todo,
        status_by: dudes_proto::Actor::System,
        seq: dudes_types::Seq(0),
        claimed_by: None,
        deps: Vec::new(),
        ticket: None,
    };
    let (mut fits, mut refused) = (0, crate::floor_websocket::MAX_MESSAGE_BYTES);
    while refused - fits > 1 {
        let candidate = fits + (refused - fits) / 2;
        card.title = "x".repeat(candidate);
        if crate::board::admit_cards(snapshot.agents.clone(), vec![card.clone()]).is_ok() {
            fits = candidate;
        } else {
            refused = candidate;
        }
    }
    card.title = "x".repeat(fits);
    crate::board::admit_cards(snapshot.agents, vec![card.clone()]).unwrap();
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::CardCreated {
                card: card.id,
                title: card.title,
                department: card.department,
                by: card.status_by,
                deps: card.deps,
            },
        })
        .unwrap();
}

#[test]
fn existing_browser_and_operator_look_admission_preserves_chat_reservation() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut initial = common::start(daemon);
    let agent = common::hire(&initial.handle, fixture.state.path());
    initial.stop(ExitCause::Ended);
    chat_capacity_history(&fixture, agent);
    let mut settings = fixture.settings();
    settings.floor_control = crate::settings::FloorControl::On {
        goal: false,
        retire: false,
        renew: false,
    };
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    let mut enabled = common::start(daemon);
    let role = "\"".repeat(512);
    let before = fixture.entries().len();
    let request = crate::floor_look::LookRequest::parse(&serde_json::to_vec(&serde_json::json!({ "request_id":"A".repeat(43), "agent":agent.to_string(), "expected_lsn":"0", "role":role, "character":null, "accent":null })).unwrap()).unwrap();
    assert!(matches!(
        enabled.handle.look(session(), request).unwrap(),
        crate::floor_look::LookOutcome::Refused {
            reason: "floor_limit",
            ..
        }
    ));
    let edit = dudes_proto::rpc::agent::LookParams {
        agent,
        role: role.clone(),
        character: None,
        accent: None,
    };
    assert!(
        enabled
            .handle
            .ask(crate::ports::Ask::Look(Box::new(edit.clone())))
            .is_err()
    );
    assert_eq!(
        fixture.entries().len(),
        before,
        "both refusal paths append nothing"
    );
    enabled.stop(ExitCause::Ended);
}

#[cfg(unix)]
#[test]
fn owned_managed_child_receives_note_only_stdin_and_journals_exact_reply() {
    let fixture = common::Fixture::new();
    let children = crate::ChildEnds::new();
    let host = native_chat_host::owned_chat_host(Arc::clone(&children));
    let predicates = Arc::new(common::ScriptedRunner::scripted([
        crate::predicate::PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: String::new(),
            timed_out: false,
        },
    ]));
    let settings = fixture.settings();
    let mut seams = with_owned_program(&settings, host, predicates);
    seams.children = children;
    seams.adapters =
        Adapters::new([Box::new(EchoEngine(std::env::current_exe().unwrap())) as Box<dyn Adapter>]);
    let daemon = boot_with_options(&settings, seams, RunOptions::default()).expect("owned daemon");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    fixture.await_record("observed original Done", |record| matches!(record, Record::LoopDecided { agent: owner, decision: LoopDecision::Done, .. } if *owner == agent).then_some(()));
    let note = send(
        &running.handle,
        &session(),
        agent,
        "café\nexact owned reply",
        0,
    );
    let turn = issued_note(&fixture, note);
    let reply = fixture.await_record("owned final reply", |record| match record {
        Record::TurnEnded {
            turn: ended,
            final_reply,
            usage,
            ..
        } if *ended == turn => Some((final_reply.as_ref().map(|reply| reply.text.clone()), *usage)),
        _ => None,
    });
    let expected_reply = format!(
        "reply: {}",
        dudes_proto::taught_note_task("café\nexact owned reply").unwrap()
    );
    assert_eq!(reply.0.as_deref(), Some(expected_reply.as_str()));
    assert!(
        reply.1.is_some(),
        "owned structured result must charge original turn"
    );
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::PredicateResult { goal: judged, .. } if *judged == goal)).count(), 1);
    let unrelated = common::hire(&running.handle, fixture.state.path());
    let mut unrelated_goal = common::goal_params(unrelated, &[common::false_program()]);
    unrelated_goal.text = "unrelated final sentinel".to_owned();
    unrelated_goal.hop_cap = 1;
    common::set_goal(&running.handle, unrelated_goal);
    fixture.await_record("unrelated ordinary final", |record| matches!(record, Record::TurnEnded { agent: owner, last_message: Some(message), .. } if *owner == unrelated && message.contains("unrelated final sentinel")).then_some(()));
    let pids: Vec<_> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::TurnSpawned { pid, .. } => Some(*pid),
            _ => None,
        })
        .collect();
    running.stop(ExitCause::Ended);
    for pid in pids {
        assert!(common::finished(pid), "owned stand-in child must be reaped");
    }
    let projection =
        dudes_journal::Projections::open(&fixture.state.path().join(crate::boot::PROJECTION_FILE))
            .expect("closed projection");
    let chat = projection.chat().expect("durable Chat history");
    let final_row = chat
        .turns
        .iter()
        .find(|row| row.turn_id == turn)
        .expect("matched note turn");
    assert_eq!(final_row.reply.as_deref(), Some(expected_reply.as_str()));
    assert_eq!(final_row.truncated, Some(false));
    assert_eq!(chat.turns.len(), 1);
    assert!(!format!("{chat:?}").contains("unrelated final sentinel"));
    let charged = projection
        .budget_by_goal_rows()
        .unwrap()
        .into_iter()
        .find(|(original, _)| *original == goal)
        .unwrap()
        .1;
    assert_eq!(charged.usd, dudes_types::Usd::from_cents(50));
    assert_acknowledged_prefixes(&fixture);
}

#[test]
fn the_exit_sequence_closes_browser_delivery() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let mut daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default(),
    )
    .unwrap();
    assert!(daemon.sup.browser_delivery_open());
    daemon.exit(ExitCause::Ended);
    assert!(!daemon.sup.browser_delivery_open());
}

fn mcp_grant_before_preparation(fixture: &common::Fixture, turn: TurnId) -> Lsn {
    fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::McpLaunchGranted {
                turn: granted,
                mcp_servers,
                ..
            } if granted == turn => {
                assert_eq!(
                    mcp_servers,
                    [dudes_types::McpServerName::parse("a").unwrap()]
                );
                Some(lsn)
            }
            _ => None,
        })
        .expect("a durable grant precedes preparation")
}

fn assert_mcp_prepared_material(host: &common::SharedHost, edited: bool) {
    let launches = host.spawned();
    assert_eq!(launches.len(), 1);
    let config = launches[0]
        .argv
        .windows(2)
        .find(|words| words[0] == "--mcp-config");
    if edited {
        let config = config.expect("the granted nonempty set has an MCP config")[1]
            .to_str()
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(config).unwrap();
        assert_eq!(
            config["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["a"]
        );
    } else {
        assert!(
            config.is_none(),
            "the ungranted empty launch has no MCP config"
        );
    }
}

#[test]
fn mcp_first_edit_waits_for_ungranted_browser_preparation_and_later_edits_keep_the_grant() {
    for edited in [false, true] {
        let fixture = common::Fixture::new();
        let host = common::SharedHost::new();
        let pause = crate::BrowserLaunchPause::default();
        pause.arm();
        let definitions = dudes_providers::mcp::parse_servers(
            toml::from_str(
                "[a]\nurl='http://127.0.0.1:4501/mcp'\n[b]\nurl='http://127.0.0.1:4502/mcp'\n",
            )
            .unwrap(),
        )
        .unwrap();
        let settings = fixture.settings();
        let daemon = crate::boot::boot_with_preparation_pause(
            &settings,
            with_owned_program(
                &settings,
                host.clone(),
                Arc::new(common::ScriptedRunner::default()),
            ),
            RunOptions::default().with_mcp_servers(definitions),
            pause.clone(),
        )
        .unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let edit = |names: Option<&[&str]>| {
            crate::ports::Ask::Mcp(dudes_proto::rpc::agent::McpParams {
                agent,
                mcp_servers: names.map(|names| {
                    names
                        .iter()
                        .map(|name| dudes_types::McpServerName::parse(name).unwrap())
                        .collect()
                }),
            })
        };
        if edited {
            running.handle.ask(edit(Some(&["a"]))).unwrap();
        }
        let note = send(
            &running.handle,
            &session(),
            agent,
            "one finite MCP fixture turn",
            0,
        );
        let turn = issued_note(&fixture, note);
        assert!(
            pause.wait(Duration::from_secs(5)),
            "the real preparation worker is held"
        );
        let before = fixture.records();
        assert!(matches!(
            running.handle.ask(edit(None)).unwrap(),
            crate::ports::Answered::Mcp(_)
        ));
        if edited {
            let grant = mcp_grant_before_preparation(&fixture, turn);
            assert!(
                matches!(running.handle.ask(edit(Some(&["b"]))).unwrap(), crate::ports::Answered::Mcp(result) if result.changed)
            );
            assert!(host.spawned().is_empty());
            pause.release();
            assert!(pause.wait_returned(1, Duration::from_secs(5)));
            fixture.await_record("spawn keeps the granted set", |record| matches!(record,
                Record::TurnSpawned { turn: spawned, mcp_grant: Some(actual), .. } if *spawned == turn && *actual == grant
            ).then_some(()));
        } else {
            assert!(
                matches!(running.handle.ask(edit(Some(&[]))).unwrap(), crate::ports::Answered::Mcp(result) if !result.changed)
            );
            let error = running.handle.ask(edit(Some(&["a"]))).unwrap_err();
            assert_eq!(error.code, dudes_proto::rpc::ErrorCode::Refused.code());
            assert_eq!(
                error.message,
                "agent.mcp: an ungranted browser launch is still being prepared or dispatched"
            );
            assert_eq!(fixture.records(), before);
            pause.release();
            assert!(pause.wait_returned(1, Duration::from_secs(5)));
            fixture.await_record("legacy preparation has settled", |record| {
                matches!(record,
                    Record::TurnSpawned { turn: spawned, mcp_grant: None, .. } if *spawned == turn
                )
                .then_some(())
            });
            assert!(
                matches!(running.handle.ask(edit(Some(&["a"]))).unwrap(), crate::ports::Answered::Mcp(result) if result.changed)
            );
        }
        assert_mcp_prepared_material(&host, edited);
        running.stop(ExitCause::Ended);
    }
}

#[test]
fn mcp_first_edit_waits_for_a_historical_ungranted_dispatch_held_across_restart() {
    let history = HistoricalReplay::recorded(true);
    let issue = history.before.browser_issue(history.turn).unwrap();
    assert!(!issue.dispatch_intents.is_empty());
    assert!(issue.spawn_lsn.is_none() && issue.end_lsn.is_none() && issue.cancellation.is_none());
    let definitions = dudes_providers::mcp::parse_servers(
        toml::from_str("[a]\nurl='http://127.0.0.1:4501/mcp'\n").unwrap(),
    )
    .unwrap();
    let host = common::SharedHost::new();
    let settings = history.fixture.settings();
    let daemon = boot_with_options(
        &settings,
        with_owned_program(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
        ),
        RunOptions::default().with_mcp_servers(definitions),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let edit = |names: Option<Vec<dudes_types::McpServerName>>| {
        crate::ports::Ask::Mcp(dudes_proto::rpc::agent::McpParams {
            agent: history.agent,
            mcp_servers: names,
        })
    };
    let before = history.fixture.records();
    for names in [None, Some(Vec::new())] {
        assert!(matches!(
            running.handle.ask(edit(names)).unwrap(),
            crate::ports::Answered::Mcp(result) if !result.changed && result.mcp_servers.is_empty()
        ));
    }
    assert_eq!(history.fixture.records(), before);
    assert!(host.spawned().is_empty(), "the halt prevents a reissue");
    let selected = vec![dudes_types::McpServerName::parse("a").unwrap()];
    let refusal = running
        .handle
        .ask(edit(Some(selected.clone())))
        .unwrap_err();
    assert_eq!(refusal.code, dudes_proto::rpc::ErrorCode::Refused.code());
    assert_eq!(
        refusal.message,
        "agent.mcp: an ungranted browser launch is still being prepared or dispatched"
    );
    assert_eq!(history.fixture.records(), before);
    let outcome = running
        .handle
        .chat(
            session(),
            request(
                ChatVerb::Cancel,
                history.agent,
                &history.note.0.to_string(),
                1,
            ),
        )
        .unwrap();
    assert!(matches!(outcome, ChatOutcome::ReissueCancelled { .. }));
    assert!(matches!(
        running.handle.ask(edit(Some(selected))).unwrap(),
        crate::ports::Answered::Mcp(result) if result.changed
    ));
    assert!(host.spawned().is_empty(), "the edit never resumes the halt");
    running.stop(ExitCause::Ended);
}

#[test]
fn an_injected_direct_browser_ack_fault_stops_before_host_dispatch() {
    for fault in [false, true] {
        let history = HistoricalReplay::new();
        let host = BoundaryHost::new();
        let pause = crate::BrowserLaunchPause::default();
        pause.arm();
        let mut settings = history.fixture.settings();
        settings.journal.checkpoint_every = 1;
        let mut daemon = crate::boot::boot_with_preparation_pause(
            &settings,
            with_owned_program(
                &settings,
                host.clone(),
                Arc::new(common::ScriptedRunner::default()),
            ),
            RunOptions::default(),
            pause.clone(),
        )
        .unwrap();
        assert!(!pause.wait(Duration::ZERO));
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        consent_to_direct_reissue(&mut daemon, &history.fixture, history.agent, history.turn);
        let parked = drive_preparation_pause(&mut daemon, &pause);
        assert!(pause.wait(Duration::from_secs(5)));
        pause.release();
        assert!(pause.wait_returned(1, Duration::from_secs(5)));
        let input = parked.unwrap_or_else(|| actual_preparation(&mut daemon));
        let dudes_supervisor::Input::BrowserLaunchReady {
            turn,
            generation,
            prepared,
        } = input
        else {
            panic!("actual prepared browser launch")
        };
        daemon.ctx.resolved_browser_turn = Some((turn, generation));
        daemon
            .sup
            .handle(
                dudes_supervisor::Input::BrowserLaunchReady {
                    turn,
                    generation,
                    prepared,
                },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
        let effects = daemon.sup.take_effects();
        let [effect @ dudes_supervisor::Effect::Persist { entry, .. }] = effects.as_slice() else {
            panic!("the actual dispatch intent is parked")
        };
        assert!(
            matches!(entry.record, Record::BrowserNoteDispatchIntent { turn: issued, .. } if issued == turn)
        );
        let before = daemon.sup.world().last_applied_lsn;
        let lsn = daemon.ctx.writer.next_lsn().unwrap();
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        if fault {
            daemon.sup.refuse_fold_once(
                lsn,
                dudes_proto::recovery_state::RecoveryStateError::Binding,
            );
        }
        let mut queue = std::collections::VecDeque::new();
        let result = daemon.ctx.dispatch_browser_intent(
            effect,
            &mut queue,
            &mut daemon.sup,
            &mut daemon.closing,
        );
        daemon.ctx.resolved_browser_turn = None;
        if fault {
            let failure = result.unwrap_err();
            assert_eq!(
                failure.downcast_ref::<dudes_supervisor::input::DurableInputError>(),
                Some(&dudes_supervisor::input::DurableInputError::Recovery {
                    lsn,
                    reason: dudes_proto::recovery_state::RecoveryStateError::Binding
                })
            );
            assert_eq!(daemon.sup.world().last_applied_lsn, before);
            assert!(daemon.ctx.journal_writes_stopped());
            assert_eq!(host.calls.load(Ordering::SeqCst), 0);
            assert!(queue.is_empty());
            assert!(daemon.sup.effects().is_empty());
        } else {
            result.unwrap();
            let through = daemon.ctx.projections.last_applied_lsn();
            assert!(through > lsn);
            assert_eq!(daemon.sup.world().last_applied_lsn, Some(through));
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            assert!(
                matches!(queue.front(), Some(dudes_supervisor::Input::Spawned { turn: issued, .. }) if *issued == turn)
            );
            for input in queue {
                daemon.pump_runtime_input(input).unwrap();
            }
        }
        let mut running = common::start(daemon);
        running.stop(ExitCause::Ended);
    }
}
